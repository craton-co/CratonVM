use super::*;

fn target(class: &str, kind: u8, owner_class_id: u32) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: class.to_string(),
        method_name: "m".to_string(),
        descriptor: "()I".to_string(),
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 21,
        target_class_id: 21,
        resolved_class_id: 0,
        owner_class_id,
        direct_entry: None,
    }
}

