/// FUNNEL2-19-3: the claim is the two proven bodies, by identity, and
/// nothing that may run Java or read a reference across an allocation.
#[test]
fn only_the_proven_bodies_claim_a_fresh_result() {
    assert!(super::is_fresh_object_result_native(
        super::lang_string::native_string_value_of_int
    ));
    assert!(super::is_fresh_object_result_native(
        super::lang_string::native_string_value_of_long
    ));
    assert!(!super::is_fresh_object_result_native(
        super::native_object_get_class
    ));
    assert!(!super::is_fresh_object_result_native(
        super::native_object_to_string
    ));
}
