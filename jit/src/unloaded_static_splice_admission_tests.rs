use super::*;

#[test]
fn an_unloaded_static_call_requires_a_precise_chain_and_its_own_proof() {
    if !ir_splice_refuse_unbindable_call_enabled() {
        return;
    }
    let mut nested = NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: InlineSite {
            callee_code: vec![0x1a, 0xb8, 0, 1, 0xac],
            callee_code_len: 5,
            callee_max_locals: 1,
            callee_num_args: 1,
            callee_is_static: true,
            return_type: b'I',
            unloaded_static_call_pcs: vec![1],
            invoke_targets: vec![(
                1,
                InlineInvokeTarget {
                    class_name: "Unloaded".into(),
                    method_name: "cold".into(),
                    descriptor: "(I)I".into(),
                    invoke_kind: 3,
                    num_jit_args: 1,
                    return_type: b'I',
                    ..Default::default()
                },
            )],
            ..Default::default()
        },
        bimorphic_second: None,
    };
    assert!(!ir_splice_nested_entry_refused(&nested, None, true));
    assert!(ir_splice_nested_entry_refused(&nested, None, false));
    nested.site.unloaded_static_call_pcs = vec![0];
    assert!(ir_splice_nested_entry_refused(&nested, None, true));
    nested.site.unloaded_static_call_pcs.clear();
    assert!(ir_splice_nested_entry_refused(&nested, None, true));
}
