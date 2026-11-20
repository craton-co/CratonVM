use super::concat_row_shape;

#[test]
fn splicecat_concat_row_shapes_count_one_entry_per_value() {
    assert_eq!(concat_row_shape("(I)Ljava/lang/String;"), Some((1, b'L')));
    assert_eq!(
        concat_row_shape("(CFDCLjava/lang/Object;J)Ljava/lang/String;"),
        Some((6, b'L'))
    );
    assert_eq!(concat_row_shape("()Ljava/lang/String;"), None, "the constant-word path");
    assert_eq!(concat_row_shape("(I)I"), None, "never a concat call type");
}
