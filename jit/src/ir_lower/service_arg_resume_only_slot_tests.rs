use super::reference_parameter_slots;

#[test]
fn uses_reference_parameter_slots_and_keeps_category_two_offsets() {
    assert_eq!(
        reference_parameter_slots("([BI)I", 2),
        Some(vec![0])
    );
    assert_eq!(
        reference_parameter_slots("(Ljava/lang/Object;J)V", 4),
        Some(vec![0, 1])
    );
    assert_eq!(reference_parameter_slots("([BI)I", 1), None);
}
