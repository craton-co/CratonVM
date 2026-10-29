// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5, lane RECORDS (RECORDS21-3): the record fold's two reference
//! shortcuts -- a `String` component's cached `hash` ahead of the
//! `Objects.hashCode` call, and `a == b` ahead of the `Objects.equals` call.
//! See `record_fold_shortcuts.rs`.

use super::*;
use cratonvm_types::flags::with_thread_overrides;
use std::collections::HashMap;

/// `0: aload_0  1: invokedynamic #1 (R)I  6: ireturn`.
const HASH: [u8; 9] = [0x2a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
/// `0: aload_0  1: aload_1  2: invokedynamic #1 (R,Object)Z  7: ireturn`.
const EQUALS: [u8; 10] = [0x2a, 0x2b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
const RECORD: u32 = 0x4321;
/// A stand-in `JitInvokeInfo` address: the builder only carries it.
const OBJECTS_CALL: usize = 0x1000;
/// A stand-in `String.hash` slot, distinct from every component index here.
const HASH_SLOT: u32 = 7;
const STRING_HASH: &str = "CRATONVM_JIT_IR_INDY_RECORD_FOLD_STRING_HASH";
const IDENTITY: &str = "CRATONVM_JIT_IR_INDY_RECORD_FOLD_REF_IDENTITY";

fn fold(
    equals: bool,
    components: &[(u32, u8)],
    strings: &[bool],
    slot: Option<u32>,
) -> IndyRecordFold {
    IndyRecordFold {
        equals,
        record_class_id: RECORD,
        components: components.to_vec(),
        string_components: strings.to_vec(),
        string_hash_slot: slot,
    }
}

fn build(row: IndyRecordFold) -> Graph {
    let (code, len, pc, args, ret) = if row.equals {
        (&EQUALS[..], 8usize, 2usize, 2usize, b'Z')
    } else {
        (&HASH[..], 7, 1, 1, b'I')
    };
    let mut b = IrBuilder::new(args, args);
    b.set_param_types(&vec![IrType::Ref; args]);
    b.set_wide_field_gates(true, true);
    b.set_indy_trap_sites(HashMap::from([(pc, (args, ret))]));
    b.set_invoke_info(HashMap::from([(pc, (OBJECTS_CALL, args, ret))]));
    b.set_indy_record_folds(HashMap::from([(pc, row)]));
    b.build(code, len).expect("builds")
}

fn calls(g: &Graph) -> Vec<NodeId> {
    (0..g.nodes.len())
        .filter(|&i| {
            g.nodes[i].op
                == Op::Call {
                    info_ptr: OBJECTS_CALL,
                }
        })
        .map(|i| i as NodeId)
        .collect()
}

/// The `Int` loads of slot [`HASH_SLOT`] whose base is a component's
/// reference load: the cached-hash reads.
fn cached_hash_loads(g: &Graph) -> Vec<NodeId> {
    (0..g.nodes.len())
        .filter(|&i| {
            let n = &g.nodes[i];
            n.op == Op::Load(MemKind::Int)
                && g.nodes[n.inputs[3] as usize].op == Op::Const(i64::from(HASH_SLOT))
                && g.nodes[n.inputs[2] as usize].op == Op::Load(MemKind::Ref)
        })
        .map(|i| i as NodeId)
        .collect()
}

fn merges(g: &Graph, inputs: usize) -> usize {
    g.nodes
        .iter()
        .filter(|n| n.op == Op::Merge && n.inputs.len() == inputs)
        .count()
}

/// An `Int` `Phi` whose inputs hold every one of `values`.
fn int_phi_over(g: &Graph, values: &[NodeId]) -> bool {
    g.nodes.iter().any(|n| {
        n.op == Op::Phi && n.ty == IrType::Int && values.iter().all(|v| n.inputs.contains(v))
    })
}

#[test]
fn a_string_component_reads_its_cached_hash_before_the_objects_call() {
    let g = build(fold(
        false,
        &[(0, b'I'), (1, b'L')],
        &[false, true],
        Some(HASH_SLOT),
    ));
    let loads = cached_hash_loads(&g);
    assert_eq!(loads.len(), 1, "one cached-hash read");
    let calls = calls(&g);
    assert_eq!(calls.len(), 1, "the call stays, on the uncached edge");
    assert_eq!(merges(&g, 3), 1, "null, cached and called edges join");
    assert!(
        int_phi_over(&g, &[loads[0], calls[0]]),
        "the component hash is the cached value or the call's"
    );
    assert_eq!(g.record_fold_call_pcs, vec![1], "still no post-call exit");
}

#[test]
fn only_a_string_typed_component_with_a_known_slot_reads_the_cache() {
    for row in [
        // A reference component that is not a `String`.
        fold(false, &[(0, b'L')], &[false], Some(HASH_SLOT)),
        // No marks at all (the VM marked none).
        fold(false, &[(0, b'L')], &[], Some(HASH_SLOT)),
        // A `String` component, but no `String.hash` slot was admitted.
        fold(false, &[(0, b'L')], &[true], None),
    ] {
        let g = build(row);
        assert!(cached_hash_loads(&g).is_empty());
        assert_eq!(calls(&g).len(), 1, "the plain Objects.hashCode call");
        assert_eq!(merges(&g, 3), 0);
    }
}

#[test]
fn equals_tests_the_same_object_before_objects_equals() {
    let row = || fold(true, &[(0, b'L')], &[], None);
    with_thread_overrides(&[(IDENTITY, None)], || {
        let g = build(row());
        let calls = calls(&g);
        assert_eq!(calls.len(), 1);
        // The call's control is the "distinct objects" edge of an `If` over
        // `Cmp(Eq)` of the two component loads.
        let ctrl = &g.nodes[g.nodes[calls[0] as usize].inputs[0] as usize];
        assert_eq!(ctrl.op, Op::Proj(1));
        let iff = &g.nodes[ctrl.inputs[0] as usize];
        assert_eq!(iff.op, Op::If);
        let cmp = &g.nodes[iff.inputs[1] as usize];
        assert_eq!(cmp.op, Op::Cmp(CmpOp::Eq));
        assert!(cmp
            .inputs
            .iter()
            .all(|&v| g.nodes[v as usize].op == Op::Load(MemKind::Ref)));
        assert_eq!(merges(&g, 2), 1, "same object and called edges join");
        // The component's answer: `true` on the same-object edge, else the call's.
        assert!(g.nodes.iter().any(|n| {
            n.op == Op::Phi
                && n.ty == IrType::Int
                && n.inputs.len() == 3
                && n.inputs[2] == calls[0]
                && g.nodes[n.inputs[1] as usize].op == Op::Const(1)
        }));
        // Same object, not an instance, the difference, all equal.
        assert_eq!(merges(&g, 4), 1);
    });
    with_thread_overrides(&[(IDENTITY, Some("0"))], || {
        let g = build(row());
        assert_eq!(calls(&g).len(), 1);
        assert_eq!(merges(&g, 2), 0, "=0 calls Objects.equals directly");
        assert_eq!(merges(&g, 4), 1);
    });
}

#[test]
fn the_string_hash_slot_is_admitted_only_for_a_marked_fold_and_under_the_switch() {
    let marked = fold(false, &[(0, b'L')], &[true], None);
    let unmarked = fold(false, &[(0, b'L')], &[false], None);
    let admit = |value: Option<&str>, field: Option<(u32, usize)>| {
        with_thread_overrides(&[(STRING_HASH, value)], || {
            let mut folds = HashMap::from([(1usize, marked.clone()), (9usize, unmarked.clone())]);
            record_fold_admit_string_hash(&mut folds, field);
            (folds[&1].string_hash_slot, folds[&9].string_hash_slot)
        })
    };
    let field = Some((0x77, HASH_SLOT as usize));
    assert_eq!(admit(None, field), (Some(HASH_SLOT), None), "default ON");
    assert_eq!(admit(Some("0"), field), (None, None), "=0 kills it");
    assert_eq!(admit(None, None), (None, None), "no String.hash answer");
}

#[test]
fn both_switches_are_default_on_and_zero_kills_them() {
    let rows: [(&str, fn() -> bool); 2] = [
        (STRING_HASH, ir_indy_record_fold_string_hash_enabled),
        (IDENTITY, ir_indy_record_fold_ref_identity_enabled),
    ];
    for (key, read) in rows {
        assert!(with_thread_overrides(&[(key, None)], read), "{key} default");
        assert!(!with_thread_overrides(&[(key, Some("0"))], read), "{key}=0");
    }
}
