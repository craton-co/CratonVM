use super::*;

#[test]
fn ambiguous_is_neither_unique_nor_absent() {
    let ambiguous = NameLookup::Ambiguous { definitions: 2 };
    assert!(ambiguous.is_ambiguous());
    assert_eq!(ambiguous.unique(), None);

    let absent = NameLookup::Absent;
    assert!(!absent.is_ambiguous());
    assert_eq!(absent.unique(), None);
    assert_ne!(
        absent, ambiguous,
        "the two must not compare equal — collapsing them is the bug",
    );

    let unique = NameLookup::Unique(ClassId::new(7));
    assert!(!unique.is_ambiguous());
    assert_eq!(unique.unique(), Some(ClassId::new(7)));
}

#[test]
fn the_message_names_the_class_and_the_count() {
    let err = ClassIdentityError::AmbiguousName {
        name: "p/X".to_string(),
        definitions: 3,
    };
    let rendered = err.to_string();
    assert!(rendered.contains("p/X"), "{rendered}");
    assert!(rendered.contains('3'), "{rendered}");
    assert_eq!(err.name(), "p/X");
    assert!(err.is_ambiguous());
}

/// The two refusals must stay in different `VmError` families: a caller
/// that unwraps the conversion has to be able to tell "there is no such
/// class" from "there are two of them".
#[test]
fn the_two_refusals_convert_to_different_vm_errors() {
    let ambiguous: VmError = ClassIdentityError::AmbiguousName {
        name: "p/X".to_string(),
        definitions: 2,
    }
    .into();
    assert!(
        matches!(
            ambiguous,
            VmError::Linkage(LinkageError::IncompatibleClassChangeError { .. })
        ),
        "{ambiguous:?}",
    );
    assert!(ambiguous.to_string().contains("p/X"));

    let refused: VmError = ClassIdentityError::Refused {
        name: "p/X".to_string(),
        reason: "--real-jdk".to_string(),
    }
    .into();
    assert!(
        matches!(
            refused,
            VmError::ClassFile(ClassFileError::ClassNotFound { .. })
        ),
        "{refused:?}",
    );
}

/// The trait methods must be invisible to a context that does not override
/// them: the fabrication default keeps answering `ClassId::new(0)`, the
/// VM-internal door answers the same, and the classifier reports what
/// `class_id_by_name` reports.
///
/// The infallible `ensure_synthetic_class` this also covered was deleted by
/// Real-JDK wave 2 step 3 (2026-08-10); the remaining two are what a mock
/// or non-VM context now sees.
#[test]
fn the_trait_defaults_preserve_the_previous_answers() {
    use crate::registry::{NativeClassAccess, NativeSystemAccess};
    use crate::test_mock::MockNativeContext;

    let mut ctx = MockNativeContext::new();

    assert_eq!(
        ctx.try_ensure_synthetic_class("p/Whatever", 4),
        Ok(ClassId::new(0)),
    );
    assert_eq!(
        ctx.ensure_vm_internal_class("p/Whatever", 4),
        ClassId::new(0),
    );

    // The mock has no name index, so every name reads absent — the same
    // answer its `class_id_by_name` gives. A context that cannot detect
    // ambiguity must not claim to.
    assert_eq!(ctx.class_id_by_name("p/Whatever"), None);
    assert_eq!(ctx.classify_class_name("p/Whatever"), NameLookup::Absent);
}

#[test]
fn the_error_travels_through_a_native_return_type() {
    // The shape every migrated call site takes: the VM answers with a
    // `ClassIdentityError`, the native propagates it with `?`.
    fn vm_answer() -> Result<ClassId, ClassIdentityError> {
        Err(ClassIdentityError::AmbiguousName {
            name: "p/X".to_string(),
            definitions: 2,
        })
    }
    fn migrated_native() -> Result<ClassId, MethodCallFailed> {
        Ok(vm_answer()?)
    }
    let failed = migrated_native().expect_err("the refusal propagates");
    assert!(matches!(failed, MethodCallFailed::InternalError(_)));
    assert!(failed.to_string().contains("p/X"));
}
