use super::declares_fill_override_bridge;
use cratonvm_native_api::MethodMetadata;
use cratonvm_types::ClassId;

fn method(name: &str, descriptor: &str, access_flags: u16) -> MethodMetadata {
    MethodMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags,
        declaring_class_id: ClassId::new(7),
        exceptions: Vec::new(),
        signature: None,
    }
}

#[test]
fn a_covariant_override_to_a_non_ancestor_is_found_through_its_bridge() {
    // `class U extends RuntimeException { public Error fillInStackTrace() }`:
    // javac emits the real method and an ACC_BRIDGE|ACC_SYNTHETIC twin.
    let methods = vec![
        method("<init>", "()V", 0x0000),
        method("fillInStackTrace", "()Ljava/lang/Error;", 0x0001),
        method("fillInStackTrace", "()Ljava/lang/Throwable;", 0x1041),
    ];
    assert!(declares_fill_override_bridge(&methods));
}

#[test]
fn no_override_and_a_static_lookalike_are_not_overrides() {
    let none = vec![
        method("<init>", "()V", 0x0000),
        method("getMessage", "()Ljava/lang/String;", 0x0001),
    ];
    assert!(!declares_fill_override_bridge(&none));
    let static_one = vec![method(
        "fillInStackTrace",
        "()Ljava/lang/Throwable;",
        0x0009,
    )];
    assert!(!declares_fill_override_bridge(&static_one));
    let other_desc = vec![method("fillInStackTrace", "()Ljava/lang/Error;", 0x0001)];
    assert!(!declares_fill_override_bridge(&other_desc));
}
