// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Why a JVMTI redefinition was refused, in the terms HotSpot reports it.
//!
//! `ClassManager::redefine_class` refuses a class file that does not parse,
//! names another class, changes the class's shape or fails verification. Until
//! interpreter round i1 wave 27 (lane L3) the refusal was one
//! `LinkageError::UnsupportedClassRedefinitionError` with a free-text message,
//! and `java.lang.instrument`'s natives logged it and returned normally, so an
//! agent never learnt that its change was not installed
//! (`docs/internal/fixed-bugs/interpreter-L3-a-rejected-redefinition-returns-normally-FIXED-20260928.md`).
//!
//! [`RedefinitionRefused`] carries the reason as a [`RedefinitionRefusal`],
//! one per JVMTI error code HotSpot's `VM_RedefineClasses` returns, and
//! [`RedefinitionRefusal::instrument_throwable`] is libinstrument's mapping of
//! that code to the Java throwable (`JPLISAgent.c`, `redefineClassMapper`).
//! The texts are the ones HotSpot 25 printed for
//! `tools/probes/interp/L3/L3W27RedefinitionRefusals.java`.

use cratonvm_types::error::LinkageError;

/// The JVMTI error a refused redefinition maps to (HotSpot's
/// `VM_RedefineClasses::load_new_class_versions` /
/// `compare_and_normalize_class_versions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedefinitionRefusal {
    /// `JVMTI_ERROR_INVALID_CLASS_FORMAT`: too short, bad magic, does not parse.
    InvalidClassFormat,
    /// `JVMTI_ERROR_NAMES_DONT_MATCH`: the bytes declare another class.
    NamesDontMatch,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_HIERARCHY_CHANGED`: superclass or
    /// direct interfaces.
    HierarchyChanged,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_ATTRIBUTE_CHANGED`: the
    /// `NestHost`, `NestMembers`, `Record` or `PermittedSubclasses`
    /// attribute (interpreter round i1 wave 45, lane L3;
    /// `tools/probes/interp/L3/L3W45RedefineShapes.java`).
    ClassAttributeChanged,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_MODIFIERS_CHANGED`.
    ClassModifiersChanged,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_SCHEMA_CHANGED`: a field added,
    /// removed, renamed, retyped or with other modifiers.
    SchemaChanged,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_ADDED`.
    MethodAdded,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_DELETED`.
    MethodDeleted,
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_METHOD_MODIFIERS_CHANGED`.
    MethodModifiersChanged,
    /// `JVMTI_ERROR_FAILS_VERIFICATION`.
    FailsVerification,
    /// `JVMTI_ERROR_INVALID_CLASS`: the class id names no loaded class.
    InvalidClass,
}

impl RedefinitionRefusal {
    /// The throwable `Instrumentation.redefineClasses` /
    /// `retransformClasses` raises for this refusal on HotSpot: the class's
    /// internal name and its message (`None`: constructed without one, as
    /// libinstrument does for the format and verification errors).
    pub fn instrument_throwable(self) -> (&'static str, Option<&'static str>) {
        const UOE: &str = "java/lang/UnsupportedOperationException";
        match self {
            RedefinitionRefusal::InvalidClassFormat => ("java/lang/ClassFormatError", None),
            RedefinitionRefusal::NamesDontMatch => (
                "java/lang/NoClassDefFoundError",
                Some("class names don't match"),
            ),
            RedefinitionRefusal::HierarchyChanged => (
                UOE,
                Some("class redefinition failed: attempted to change superclass or interfaces"),
            ),
            RedefinitionRefusal::ClassAttributeChanged => (
                UOE,
                Some(
                    "class redefinition failed: attempted to change the class NestHost, \
                     NestMembers, Record, or PermittedSubclasses attribute",
                ),
            ),
            RedefinitionRefusal::ClassModifiersChanged => (
                UOE,
                Some("class redefinition failed: attempted to change the class modifiers"),
            ),
            RedefinitionRefusal::SchemaChanged => (
                UOE,
                Some(
                    "class redefinition failed: attempted to change the schema (add/remove fields)",
                ),
            ),
            RedefinitionRefusal::MethodAdded => (
                UOE,
                Some("class redefinition failed: attempted to add a method"),
            ),
            RedefinitionRefusal::MethodDeleted => (
                UOE,
                Some("class redefinition failed: attempted to delete a method"),
            ),
            RedefinitionRefusal::MethodModifiersChanged => (
                UOE,
                Some("class redefinition failed: attempted to change method modifiers"),
            ),
            RedefinitionRefusal::FailsVerification => ("java/lang/VerifyError", None),
            RedefinitionRefusal::InvalidClass => ("java/lang/InternalError", None),
        }
    }
}

/// A refused redefinition: why ([`RedefinitionRefusal`]), of which class, and
/// CratonVM's own account of it (`message`, for the log; the Java-visible text
/// is [`RedefinitionRefusal::instrument_throwable`]'s).
#[derive(Debug, Clone)]
pub struct RedefinitionRefused {
    pub kind: RedefinitionRefusal,
    pub class_name: String,
    pub message: String,
}

impl RedefinitionRefused {
    pub fn new(
        kind: RedefinitionRefusal,
        class_name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            class_name: class_name.into(),
            message: message.into(),
        }
    }

    /// The untyped form `ClassManager::redefine_class` has always returned.
    pub fn into_linkage_error(self) -> LinkageError {
        LinkageError::UnsupportedClassRedefinitionError {
            class_name: self.class_name,
            message: self.message,
        }
    }
}

impl From<RedefinitionRefused> for LinkageError {
    fn from(refused: RedefinitionRefused) -> Self {
        refused.into_linkage_error()
    }
}

#[cfg(test)]
mod tests {
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
            RedefinitionRefusal::ClassAttributeChanged.instrument_throwable().1,
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
        let refused =
            RedefinitionRefused::new(RedefinitionRefusal::MethodAdded, "p/C", "method count changed");
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
}
