use super::execute_method_index_key;
use cratonvm_types::ClassId;

#[test]
fn the_key_names_the_class_the_name_and_the_descriptor() {
    let a = execute_method_index_key(ClassId::new(7), "invokeStatic", "(Ljava/lang/Object;)V");
    assert_eq!(
        a,
        execute_method_index_key(ClassId::new(7), "invokeStatic", "(Ljava/lang/Object;)V")
    );
    assert_ne!(
        a,
        execute_method_index_key(ClassId::new(8), "invokeStatic", "(Ljava/lang/Object;)V")
    );
    assert_ne!(
        a.1,
        execute_method_index_key(ClassId::new(7), "invokeStatic", "(Ljava/lang/Object;I)V").1
    );
    // The name / descriptor split is part of the key (`str::hash` ends
    // each string with a terminator byte).
    assert_ne!(
        execute_method_index_key(ClassId::new(7), "ab", "c").1,
        execute_method_index_key(ClassId::new(7), "a", "bc").1
    );
}
