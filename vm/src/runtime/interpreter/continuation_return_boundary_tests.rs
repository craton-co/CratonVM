use super::continuation_return_needs_conversion as needs;

/// `coerce_return`'s identity cases need no adapter; every conversion
/// it performs (drop, box, unbox, widen) does.
#[test]
fn only_a_real_conversion_is_recorded() {
    assert!(!needs("V", "V"));
    assert!(!needs("I", "I"));
    assert!(!needs("Ljava/lang/Object;", "Ljava/lang/String;"));
    assert!(!needs("Ljava/lang/Object;", "[I"));
    assert!(needs("V", "Ljava/lang/Object;"));
    assert!(needs("V", "Z"));
    assert!(needs("Ljava/lang/Object;", "I"));
    assert!(needs("I", "Ljava/lang/Object;"));
    assert!(needs("J", "I"));
}
