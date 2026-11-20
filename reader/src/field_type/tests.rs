use super::*;

#[test]
fn parse_primitive_types() {
    assert_eq!(FieldType::parse("I").unwrap(), FieldType::Int);
    assert_eq!(FieldType::parse("J").unwrap(), FieldType::Long);
    assert_eq!(FieldType::parse("Z").unwrap(), FieldType::Boolean);
}

#[test]
fn parse_object_type() {
    assert_eq!(
        FieldType::parse("Ljava/lang/String;").unwrap(),
        FieldType::Object("java/lang/String".to_string())
    );
}

#[test]
fn parse_array_type() {
    assert_eq!(
        FieldType::parse("[I").unwrap(),
        FieldType::Array(Box::new(FieldType::Int))
    );
}

#[test]
fn parse_nested_array() {
    assert_eq!(
        FieldType::parse("[[Ljava/lang/Object;").unwrap(),
        FieldType::Array(Box::new(FieldType::Array(Box::new(FieldType::Object(
            "java/lang/Object".to_string()
        )))))
    );
}

#[test]
fn stack_slots() {
    assert_eq!(FieldType::Int.stack_slots(), 1);
    assert_eq!(FieldType::Long.stack_slots(), 2);
    assert_eq!(FieldType::Double.stack_slots(), 2);
    assert_eq!(FieldType::Object("Foo".to_string()).stack_slots(), 1);
}

#[test]
fn deeply_nested_array_is_rejected_not_overflow() {
    // 256 leading '[' exceeds the 255-dimension cap: must return an
    // error rather than recursing into a stack overflow.
    let descriptor = format!("{}I", "[".repeat(256));
    assert!(FieldType::parse(&descriptor).is_err());

    // Far past the cap stays an error (and does not panic/overflow).
    let huge = format!("{}I", "[".repeat(100_000));
    assert!(FieldType::parse(&huge).is_err());

    // Exactly 255 dimensions remains valid.
    let at_cap = format!("{}I", "[".repeat(255));
    assert!(FieldType::parse(&at_cap).is_ok());
}

#[test]
fn display_roundtrip() {
    let types = ["I", "J", "Ljava/lang/String;", "[I", "[[D"];
    for desc in types {
        let parsed = FieldType::parse(desc).unwrap();
        assert_eq!(parsed.to_string(), desc);
    }
}
