use super::*;

#[test]
fn parse_void_no_args() {
    let desc = MethodDescriptor::parse("()V").unwrap();
    assert!(desc.parameters.is_empty());
    assert_eq!(desc.return_type, None);
}

#[test]
fn parse_single_int_param() {
    let desc = MethodDescriptor::parse("(I)V").unwrap();
    assert_eq!(desc.parameters, vec![FieldType::Int]);
    assert_eq!(desc.return_type, None);
}

#[test]
fn parse_multiple_params_with_return() {
    let desc = MethodDescriptor::parse("(ILjava/lang/String;)Ljava/lang/Object;").unwrap();
    assert_eq!(
        desc.parameters,
        vec![
            FieldType::Int,
            FieldType::Object("java/lang/String".to_string()),
        ]
    );
    assert_eq!(
        desc.return_type,
        Some(FieldType::Object("java/lang/Object".to_string()))
    );
}

#[test]
fn parse_main_descriptor() {
    let desc = MethodDescriptor::parse("([Ljava/lang/String;)V").unwrap();
    assert_eq!(
        desc.parameters,
        vec![FieldType::Array(Box::new(FieldType::Object(
            "java/lang/String".to_string()
        )))]
    );
    assert_eq!(desc.return_type, None);
}

#[test]
fn parameter_stack_slots() {
    let desc = MethodDescriptor::parse("(IJD)V").unwrap();
    assert_eq!(desc.parameters_stack_slots(), 5); // 1 + 2 + 2
}

#[test]
fn display_roundtrip() {
    let descriptors = [
        "()V",
        "(I)I",
        "(ILjava/lang/String;)Ljava/lang/Object;",
        "([Ljava/lang/String;)V",
    ];
    for d in descriptors {
        let parsed = MethodDescriptor::parse(d).unwrap();
        assert_eq!(parsed.to_string(), d);
    }
}
