use super::{method_repeated_attribute_texts, repeated_attribute_refusal};

/// The static table answers as the per-method `Vec` did: `Code` and
/// `MethodParameters` for every version, the annotation family from 49.
#[test]
fn the_table_is_the_old_vec_by_version() {
    let refuse = |major: u16, names: &[&str]| {
        repeated_attribute_refusal(
            names.iter().copied(),
            method_repeated_attribute_texts(major),
            "C",
        )
    };
    assert_eq!(
        refuse(48, &["Code", "Code"]).as_deref(),
        Some("Multiple Code attributes in class file C")
    );
    assert_eq!(refuse(48, &["Signature", "Signature"]), None, "pre-49: not a rule");
    assert_eq!(
        refuse(49, &["Signature", "Code", "Signature"]).as_deref(),
        Some("Multiple Signature attributes for method in class file C")
    );
    assert_eq!(
        refuse(69, &["RuntimeInvisibleTypeAnnotations", "RuntimeInvisibleTypeAnnotations"])
            .as_deref(),
        Some("Multiple RuntimeInvisibleTypeAnnotations attributes for method in class file C")
    );
    assert_eq!(refuse(69, &["Code", "Exceptions", "Signature"]), None);
    assert_eq!(method_repeated_attribute_texts(48).len(), 2);
    assert_eq!(method_repeated_attribute_texts(65).len(), 10);
}
