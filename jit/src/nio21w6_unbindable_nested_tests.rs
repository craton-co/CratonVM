use super::*;

fn row(
    kind: u8,
    entry: Option<(usize, bool)>,
    class: &str,
    method: &str,
    desc: &str,
) -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: class.to_string(),
        method_name: method.to_string(),
        descriptor: desc.to_string(),
        invoke_kind: kind,
        direct_entry: entry,
        ..InlineInvokeTarget::default()
    }
}

