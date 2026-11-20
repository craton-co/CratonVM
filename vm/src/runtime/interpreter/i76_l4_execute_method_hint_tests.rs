use super::{set_execute_method_hint, take_execute_method_hint};
use cratonvm_types::ClassId;

#[test]
fn a_hint_is_taken_once_and_only_for_its_class() {
    set_execute_method_hint(ClassId::new(7), 3);
    // Another class's execute clears it without using it.
    assert_eq!(take_execute_method_hint(ClassId::new(8)), None);
    assert_eq!(take_execute_method_hint(ClassId::new(7)), None);
    set_execute_method_hint(ClassId::new(7), 3);
    assert_eq!(take_execute_method_hint(ClassId::new(7)), Some(3));
    assert_eq!(take_execute_method_hint(ClassId::new(7)), None);
}
