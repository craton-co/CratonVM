use super::definer_diagnosis;
use cratonvm_types::error::{ClassFileError, LinkageError, MethodCallFailed, VmError};

fn linkage(error: LinkageError) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Linkage(error))
}

#[test]
fn the_wrong_name_of_the_requested_class_is_an_answer() {
    let failed = linkage(LinkageError::NoClassDefFoundError {
        class_name: "Target (wrong name: p/Target)".to_string(),
    });
    assert_eq!(
        definer_diagnosis(&failed, "Target"),
        Some((
            "java/lang/NoClassDefFoundError",
            "Target (wrong name: p/Target)".to_string()
        ))
    );
    // Another name's diagnosis, or a bare NCDFE, is not.
    assert_eq!(definer_diagnosis(&failed, "Other"), None);
    let bare = linkage(LinkageError::NoClassDefFoundError {
        class_name: "Target".to_string(),
    });
    assert_eq!(definer_diagnosis(&bare, "Target"), None);
}

#[test]
fn format_and_version_refusals_keep_hotspots_text() {
    let magic = linkage(LinkageError::ClassFormatError {
        class_name: "Bad".to_string(),
        message: "Incompatible magic value 1213486160 in class file Bad".to_string(),
    });
    assert_eq!(
        definer_diagnosis(&magic, "Bad"),
        Some((
            "java/lang/ClassFormatError",
            "Incompatible magic value 1213486160 in class file Bad".to_string()
        ))
    );
    let other = linkage(LinkageError::ClassFormatError {
        class_name: "Bad".to_string(),
        message: "attribute decode failed: x".to_string(),
    });
    assert_eq!(
        definer_diagnosis(&other, "Bad").map(|(_, m)| m),
        Some("Bad: attribute decode failed: x".to_string())
    );
    // HotSpot's `Truncated class file` names no class, and is raised
    // as is (interpreter round i5 wave 109, lane L5).
    let truncated = linkage(LinkageError::ClassFormatError {
        class_name: "Bad".to_string(),
        message: "Truncated class file".to_string(),
    });
    assert_eq!(
        definer_diagnosis(&truncated, "Bad").map(|(_, m)| m),
        Some("Truncated class file".to_string())
    );
    let newer = linkage(LinkageError::UnsupportedClassVersionError {
        class_name: "N".to_string(),
        message: "N has been compiled by a more recent version".to_string(),
    });
    assert_eq!(
        definer_diagnosis(&newer, "N").map(|(t, _)| t),
        Some("java/lang/UnsupportedClassVersionError")
    );
}

#[test]
fn plain_absence_stays_absent() {
    let missing =
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: "Nope".to_string(),
        }));
    assert_eq!(definer_diagnosis(&missing, "Nope"), None);
}

/// `resolver_reports_absence` (interpreter round i2 wave 61, lane L5):
/// only a miss is absence; every refusal of a found class file is not.
#[test]
fn only_a_miss_is_the_resolvers_absence() {
    use super::resolver_reports_absence;
    assert!(resolver_reports_absence(&VmError::ClassFile(
        ClassFileError::ClassNotFound {
            class_name: "Nope".to_string(),
        }
    )));
    assert!(resolver_reports_absence(&VmError::Linkage(
        LinkageError::NoClassDefFoundError {
            class_name: "Nope".to_string(),
        }
    )));
    assert!(!resolver_reports_absence(&VmError::Linkage(
        LinkageError::NoClassDefFoundError {
            class_name: "Target (wrong name: p/Target)".to_string(),
        }
    )));
    assert!(!resolver_reports_absence(&VmError::Linkage(
        LinkageError::ClassCircularityError {
            class_name: "A".to_string(),
        }
    )));
    assert!(!resolver_reports_absence(&VmError::Linkage(
        LinkageError::IncompatibleClassChangeError {
            message: "class A has interface I as super class".to_string(),
        }
    )));
}
