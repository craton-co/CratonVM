use super::*;

fn row(kind: u8, class: &str, method: &str, desc: &str) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: class.to_string(),
        method_name: method.to_string(),
        descriptor: desc.to_string(),
        invoke_kind: kind,
        direct_entry: None,
        ..InlineInvokeTarget::default()
    }
}

// Only the static fence row counts, only while both switches hold; a
// bound row is not this rule's question.
