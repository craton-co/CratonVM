use super::{
    compile_attempt_due, value_fits_param_tag, with_bytecode_only_key, BYTECODE_ONLY_KEY_PREFIX,
};
use crate::types::Value;

#[test]
fn the_door_invisible_descriptor_is_the_prefix_and_the_descriptor() {
    let desc = "(Ljava/lang/String;Z)Ljava/lang/Class;";
    let built = with_bytecode_only_key(desc, |k| k.to_string());
    assert_eq!(built, format!("{BYTECODE_ONLY_KEY_PREFIX}{desc}"));
    assert_eq!(&*super::bytecode_only_key_descriptor(desc), built.as_str());
    // No method descriptor can equal it: those start with '('.
    assert!(!built.starts_with('('));
    // A descriptor wider than the stack buffer takes the allocating form.
    let wide = format!("({})V", "Ljava/lang/Object;".repeat(20));
    let built = with_bytecode_only_key(&wide, |k| k.to_string());
    assert_eq!(built, format!("{BYTECODE_ONLY_KEY_PREFIX}{wide}"));
}

#[test]
fn the_door_compiles_at_the_threshold_and_three_doublings_only() {
    assert_eq!(compile_attempt_due(0, 100), None);
    assert_eq!(compile_attempt_due(99, 100), None);
    assert_eq!(compile_attempt_due(100, 100), Some(true));
    assert_eq!(compile_attempt_due(150, 100), None);
    assert_eq!(compile_attempt_due(200, 100), Some(true));
    assert_eq!(compile_attempt_due(300, 100), None);
    assert_eq!(compile_attempt_due(400, 100), Some(true));
    assert_eq!(compile_attempt_due(800, 100), Some(true));
    assert_eq!(compile_attempt_due(1600, 100), Some(false));
    assert_eq!(compile_attempt_due(950, 100), Some(false));
    // A zero threshold is read as one.
    assert_eq!(compile_attempt_due(1, 0), Some(true));
}

#[test]
fn only_the_verbatim_representation_fits_a_parameter() {
    assert!(value_fits_param_tag(&Value::Int(1), b'Z'));
    assert!(!value_fits_param_tag(&Value::Long(1), b'I'));
    assert!(value_fits_param_tag(&Value::Object(None), b'L'));
    assert!(!value_fits_param_tag(&Value::Int(0), b'L'));
    assert!(!value_fits_param_tag(&Value::Uninitialized, b'['));
}
