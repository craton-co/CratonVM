use super::*;

fn static_loads(graph: &Graph) -> usize {
    graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::LoadStatic { .. }))
        .count()
}

fn build_other_class_splice(spliced_known: bool) -> (Graph, bool) {
    // Caller: 0 getstatic S; 3 invokestatic g; 6 iadd; 7 getstatic S;
    // 10 iadd; 11 ireturn; 12 pad. Callee `g` at 13: getstatic T; 16 ireturn.
    let code = [
        0xb2, 0x00, 0x02, 0xb8, 0x00, 0x01, 0x60, 0xb2, 0x00, 0x02, 0x60, 0xac, 0x00, //
        0xb2, 0x00, 0x03, 0xac,
    ];
    let mut b = IrBuilder::new(0, 1);
    b.set_invoke_info(HashMap::from([(3usize, (0usize, 0usize, b'I'))]));
    b.set_static_field_info(HashMap::from([
        (0usize, (7u32, 0usize, b'I', false)),
        (7usize, (7u32, 0usize, b'I', false)),
    ]));
    let mut tables = IrInlineTables::default();
    tables.sites.insert(
        3,
        IrInlineSite {
            base: 13,
            code_len: 4,
            num_args: 0,
            max_locals: 0,
            arg_local_slots: Vec::new(),
            returns_value: true,
            receiver_is_arg0: false,
            method_key: "P.g:()I".to_string(),
            class_id: 7,
        },
    );
    tables.static_field_info.insert(13, (8u32, 0usize, b'I', false));
    b.apply_inline_tables(tables);
    b.set_static_initialized_classes([7u32].into_iter().collect());
    if spliced_known {
        b.add_static_initialized_classes([8u32]);
    }
    let cannot_trap = b.splice_static_access_cannot_trap(13);
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    let graph = b.build(&padded, 12).expect("the splice builds");
    (graph, cannot_trap)
}

#[test]
fn spliced_read_of_initialised_other_class_keeps_caller_entry_and_cannot_trap() {
    let (unknown, unknown_no_trap) = build_other_class_splice(false);
    assert_eq!(static_loads(&unknown), 3);
    assert!(!unknown_no_trap);

    let (known, known_no_trap) = build_other_class_splice(true);
    assert_eq!(static_loads(&known), 2);
    assert!(known_no_trap);
}
