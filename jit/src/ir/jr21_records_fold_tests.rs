//! JIT round 21 wave 17 (lane records, IN16-6;
//! `CRATONVM_JIT_IR_INDY_RECORD_FOLD`): a record's generated `hashCode` /
//! `equals` `invokedynamic` built as the field-wise fold.
use super::*;
use std::collections::HashMap;

/// `0: aload_0  1: invokedynamic #1 (R)I  6: ireturn`.
const HASH: [u8; 9] = [0x2a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
/// `0: aload_0  1: aload_1  2: invokedynamic #1 (R,Object)Z  7: ireturn`.
const EQUALS: [u8; 10] = [0x2a, 0x2b, 0xba, 0x00, 0x01, 0x00, 0x00, 0xac, 0, 0];
const RECORD: u32 = 0x4321;
/// A stand-in `JitInvokeInfo` address: the builder only carries it.
const OBJECTS_CALL: usize = 0x1000;

fn fold(equals: bool, components: &[(u32, u8)]) -> IndyRecordFold {
    IndyRecordFold {
        equals,
        record_class_id: RECORD,
        components: components.to_vec(),
        string_components: Vec::new(),
        string_hash_slot: None,
    }
}

fn build(
    equals: bool,
    row: Option<IndyRecordFold>,
    invoke_row: bool,
    wide: (bool, bool),
) -> Option<Graph> {
    let (code, len, pc, args, ret) = if equals {
        (&EQUALS[..], 8usize, 2usize, 2usize, b'Z')
    } else {
        (&HASH[..], 7, 1, 1, b'I')
    };
    let mut b = IrBuilder::new(args, args);
    b.set_param_types(&vec![IrType::Ref; args]);
    b.set_wide_field_gates(wide.0, wide.1);
    b.set_indy_trap_sites(HashMap::from([(pc, (args, ret))]));
    if invoke_row {
        b.set_invoke_info(HashMap::from([(pc, (OBJECTS_CALL, args, ret))]));
    }
    if let Some(row) = row {
        b.set_indy_record_folds(HashMap::from([(pc, row)]));
    }
    b.build(code, len)
}

fn loads(g: &Graph) -> Vec<&Node> {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Load(_)))
        .collect()
}

fn calls(g: &Graph) -> usize {
    g.nodes
        .iter()
        .filter(|n| {
            n.op == Op::Call {
                info_ptr: OBJECTS_CALL,
            }
        })
        .count()
}

fn offset_of(g: &Graph, load: &Node) -> Op {
    g.nodes[load.inputs[3] as usize].op.clone()
}

#[test]
fn jr21_records_hash_code_is_loads_and_one_objects_call_per_reference() {
    let row = fold(false, &[(0, b'I'), (1, b'L'), (2, b'Z')]);
    let g = build(false, Some(row), true, (false, false)).expect("builds");
    assert!(!g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { .. })));
    assert!(g.site_trap_pcs.is_empty(), "no trap planted");
    let loads = loads(&g);
    assert_eq!(loads.len(), 3, "one load per component");
    let offsets: Vec<Op> = loads.iter().map(|l| offset_of(&g, l)).collect();
    assert_eq!(
        offsets,
        vec![Op::Const(0), Op::Const(1), Op::Const(2)],
        "the getters in order"
    );
    assert_eq!(loads[1].op, Op::Load(MemKind::Ref));
    assert_eq!(calls(&g), 1, "Objects.hashCode for the reference only");
    assert_eq!(g.record_fold_call_pcs, vec![1], "no post-call exit");
    assert!(g.synthetic_field_access_pcs.contains(&1));
}

#[test]
fn jr21_records_a_primitive_record_needs_no_call_row() {
    let row = fold(false, &[(0, b'I'), (1, b'C')]);
    let g = build(false, Some(row), false, (false, false)).expect("builds");
    assert_eq!(loads(&g).len(), 2);
    assert_eq!(calls(&g), 0);
}

#[test]
fn jr21_records_refusals_keep_the_old_arm() {
    // A reference component with no `Objects` row, a `long` without the
    // long gate, a float without the FP gate: no fold.
    for (row, wide) in [
        (fold(false, &[(0, b'L')]), (true, true)),
        (fold(false, &[(0, b'J')]), (false, true)),
        (fold(false, &[(0, b'F')]), (true, false)),
        // The equals shape planned at a hashCode site.
        (fold(true, &[(0, b'I')]), (true, true)),
    ] {
        if let Some(g) = build(false, Some(row), false, wide) {
            assert!(loads(&g).is_empty(), "the fold was refused");
        }
    }
}

#[test]
fn jr21_records_equals_tests_identity_then_the_class_then_the_last_component_first() {
    let row = fold(true, &[(0, b'I'), (1, b'J'), (2, b'L')]);
    let g = build(true, Some(row), true, (true, false)).expect("builds");
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::ExactClassIs { class_id: RECORD }));
    let loads = loads(&g);
    assert_eq!(loads.len(), 6, "two loads per component");
    let first: Vec<Op> = loads.iter().step_by(2).map(|l| offset_of(&g, l)).collect();
    assert_eq!(
        first,
        vec![Op::Const(2), Op::Const(1), Op::Const(0)],
        "the LAST getter is compared first"
    );
    assert_eq!(calls(&g), 1, "Objects.equals for the reference only");
    // Same object, not an instance, one difference per component, and
    // the all-equal edge.
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::Merge && n.inputs.len() == 6));
    assert_eq!(g.record_fold_call_pcs, vec![2]);
}
