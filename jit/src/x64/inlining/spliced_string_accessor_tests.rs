use super::*;

#[test]
fn test_spliced_string_accessor_resolution() {
    assert_eq!(
        spliced_string_accessor_entry("java/lang/String", "length", "()I"),
        Some(0)
    );
    assert_eq!(
        spliced_string_accessor_entry("java/lang/String", "isEmpty", "()Z"),
        Some(1)
    );
    assert_eq!(
        spliced_string_accessor_entry("java/lang/String", "charAt", "(I)C"),
        Some(2)
    );
    assert_eq!(
        spliced_string_accessor_entry("java/lang/CharSequence", "charAt", "(I)C"),
        Some(2)
    );
    assert_eq!(
        spliced_string_accessor_entry("java/lang/String", "substring", "(I)Ljava/lang/String;"),
        None
    );
    assert_eq!(
        spliced_string_accessor_entry("java/lang/StringBuilder", "charAt", "(I)C"),
        None
    );
}

