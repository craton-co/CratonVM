use super::*;

#[test]
fn number_values_narrow_and_widen_as_number_does() {
    assert_eq!(
        number_value_as('I', Value::Long(4_294_967_297)),
        Value::Int(1)
    );
    assert_eq!(number_value_as('I', Value::Double(3.9)), Value::Int(3));
    assert_eq!(number_value_as('B', Value::Int(300)), Value::Int(44));
    assert_eq!(number_value_as('S', Value::Long(70_000)), Value::Int(4_464));
    assert_eq!(number_value_as('J', Value::Int(-7)), Value::Long(-7));
    assert_eq!(number_value_as('J', Value::Float(f32::NAN)), Value::Long(0));
    assert_eq!(number_value_as('F', Value::Double(0.5)), Value::Float(0.5));
    assert_eq!(number_value_as('D', Value::Int(2)), Value::Double(2.0));
    assert_eq!(number_value_as('C', Value::Int(120)), Value::Int(120));
}

#[test]
fn only_the_eight_wrappers_take_the_wrapper_read() {
    for w in [
        "Integer",
        "Long",
        "Short",
        "Byte",
        "Character",
        "Boolean",
        "Float",
        "Double",
    ] {
        assert!(is_wrapper_desc(&format!("Ljava/lang/{w};")), "{w}");
    }
    for other in [
        "Ljava/lang/Object;",
        "Ljava/lang/Number;",
        "Ljava/lang/String;",
        "I",
    ] {
        assert!(!is_wrapper_desc(other), "{other}");
    }
}
