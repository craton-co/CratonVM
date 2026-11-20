use super::*;

#[test]
fn every_shape_refusal_is_an_unsupported_operation_with_hotspots_text() {
    for kind in [
        RedefinitionRefusal::HierarchyChanged,
        RedefinitionRefusal::ClassAttributeChanged,
        RedefinitionRefusal::ClassModifiersChanged,
        RedefinitionRefusal::SchemaChanged,
        RedefinitionRefusal::MethodAdded,
        RedefinitionRefusal::MethodDeleted,
        RedefinitionRefusal::MethodModifiersChanged,
    ] {
        let (class, message) = kind.instrument_throwable();
        assert_eq!(class, "java/lang/UnsupportedOperationException");
        assert!(
            message.is_some_and(|m| m.starts_with("class redefinition failed: attempted to ")),
            "{kind:?}: {message:?}"
        );
    }
}

/// HotSpot 25's text, measured (`L3W45RedefineShapes`, row
/// `nest-host-dropped`).
#[test]
fn a_class_attribute_change_names_the_four_attributes() {
    assert_eq!(
        RedefinitionRefusal::ClassAttributeChanged
            .instrument_throwable()
            .1,
        Some(
            "class redefinition failed: attempted to change the class NestHost, NestMembers, \
             Record, or PermittedSubclasses attribute"
        )
    );
}

#[test]
fn format_and_verification_refusals_carry_no_message() {
    assert_eq!(
        RedefinitionRefusal::InvalidClassFormat.instrument_throwable(),
        ("java/lang/ClassFormatError", None)
    );
    assert_eq!(
        RedefinitionRefusal::FailsVerification.instrument_throwable(),
        ("java/lang/VerifyError", None)
    );
}

#[test]
fn the_linkage_form_keeps_the_class_and_the_detail() {
    let refused = RedefinitionRefused::new(
        RedefinitionRefusal::MethodAdded,
        "p/C",
        "method count changed",
    );
    match refused.into_linkage_error() {
        LinkageError::UnsupportedClassRedefinitionError {
            class_name,
            message,
        } => {
            assert_eq!(class_name, "p/C");
            assert_eq!(message, "method count changed");
        }
        other => panic!("unexpected {other:?}"),
    }
}
