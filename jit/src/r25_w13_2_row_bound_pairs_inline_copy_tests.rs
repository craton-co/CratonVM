use super::*;


const DESC: &str = "(Ljava/lang/Object;ILjava/lang/Object;II)V";

fn copy_row() -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: "java/lang/System".to_string(),
        method_name: "arraycopy".to_string(),
        descriptor: DESC.to_string(),
        invoke_kind: 3,
        direct_entry: None,
        ..InlineInvokeTarget::default()
    }
}

// c30: the row-bound splice without the inline copy lost 22-27 %. Unset,
// the copy follows the splice switch; killed, the splice is not admitted.
