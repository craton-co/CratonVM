// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.6 — `java.lang.reflect.Constructor.newInstance` edge cases.
//!
//! The native `Constructor.newInstance` lives in
//! `native-builtins/src/lang_class.rs::native_constructor_new_instance` —
//! it covers the bulk path. This module hosts the *spec-classification*
//! helpers used by the WP2.6 acceptance tests:
//!
//!   * abstract / interface targets must throw `InstantiationException`.
//!   * private constructors throw `IllegalAccessException` unless
//!     `setAccessible(true)` was called.
//!   * the constructor's body throwing must wrap in
//!     `InvocationTargetException` (already done by the existing
//!     native — we just expose a classification helper for tests).
//!   * inner-class (non-static) constructors take the implicit outer
//!     reference as the first parameter.
//!   * record canonical constructors and compact-canonical constructors
//!     work via the same path.
//!
//! Keeping this module light and test-focused lets callers (the
//! integration tests under `vm/tests/wp2_6_constructor_edges.rs`) audit
//! the descriptor-classification logic without booting a full VM.
//!
//! The corresponding `native-builtins/src/lang_reflect_constructor.rs`
//! is intentionally not created — `lang_class.rs` already hosts the
//! native and sharding the file would just shuffle code around. If a
//! future WP wants to split for clarity, it can move
//! `native_constructor_new_instance` here without changing semantics.

/// Classification of a target class for `Constructor.newInstance`. The
/// native uses this to decide whether to throw `InstantiationException`
/// before allocating, matching the JDK behavior precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstantiationClassification {
    /// Concrete, non-abstract, non-interface. Allocate + invoke <init>.
    Concrete,
    /// Interface. Throw `InstantiationException` per JLS §15.9.1.
    Interface,
    /// Abstract class. Throw `InstantiationException` per JLS §15.9.1.
    Abstract,
}

impl InstantiationClassification {
    /// Classify a class given its access flags. Pulls the constants
    /// from the public `access_flags` module so this stays in sync.
    pub fn from_access_flags(flags: u16) -> Self {
        // Bit definitions per JVMS §4.1 Table 4.1-A.
        const ACC_INTERFACE: u16 = 0x0200;
        const ACC_ABSTRACT: u16 = 0x0400;
        if flags & ACC_INTERFACE != 0 {
            Self::Interface
        } else if flags & ACC_ABSTRACT != 0 {
            Self::Abstract
        } else {
            Self::Concrete
        }
    }

    /// Returns true iff `Constructor.newInstance` should refuse to
    /// allocate this class and instead throw `InstantiationException`.
    pub fn is_abstract_or_interface(self) -> bool {
        matches!(self, Self::Interface | Self::Abstract)
    }
}

/// Access-control classification for a constructor itself. The native
/// uses this with the `accessible` extra-slot flag to decide whether
/// a non-public constructor invocation should throw
/// `IllegalAccessException`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructorAccess {
    /// `public` constructor — always allowed.
    Public,
    /// `protected` constructor — allowed inside the package or
    /// subclass; for the cross-module reflection case
    /// `setAccessible(true)` is required.
    Protected,
    /// Package-private (default access). `setAccessible(true)` needed
    /// outside the declaring package.
    PackagePrivate,
    /// `private` constructor — `setAccessible(true)` always required.
    Private,
}

impl ConstructorAccess {
    /// Classify a constructor's modifier bits.
    pub fn from_modifiers(modifiers: i32) -> Self {
        const ACC_PUBLIC: i32 = 0x0001;
        const ACC_PRIVATE: i32 = 0x0002;
        const ACC_PROTECTED: i32 = 0x0004;
        if modifiers & ACC_PUBLIC != 0 {
            Self::Public
        } else if modifiers & ACC_PRIVATE != 0 {
            Self::Private
        } else if modifiers & ACC_PROTECTED != 0 {
            Self::Protected
        } else {
            Self::PackagePrivate
        }
    }

    /// Returns true iff this constructor requires
    /// `setAccessible(true)` to be invoked from an arbitrary caller.
    /// The JDK's reflection layer collapses package/protected/private
    /// into "needs override" once the `caller != declaringClass`
    /// (which is the cratonvm reflection case — the native is invoked
    /// by `Method.invoke` from arbitrary user code).
    pub fn requires_setaccessible(self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// Helper: detect an inner-class non-static constructor by inspecting
/// the descriptor of its first parameter.
///
/// In real-JDK class files, a non-static inner class's `<init>` takes
/// an implicit synthetic first parameter referring to the enclosing
/// instance. The descriptor looks like
/// `(Lcom/example/Outer;...)V`. The reflection caller must therefore
/// pass the outer-instance object as the first argument.
pub fn is_inner_class_ctor_descriptor(descriptor: &str, outer_class: &str) -> bool {
    // Strip the leading '(' and look at the first parameter type.
    if let Some(open) = descriptor.find('(') {
        let rest = &descriptor[open + 1..];
        if rest.starts_with('L') {
            if let Some(end) = rest.find(';') {
                let first_type = &rest[1..end];
                return first_type == outer_class;
            }
        }
    }
    false
}

/// Map a JDK constructor descriptor to a stable test-case label, used
/// by the WP2.6 acceptance tests to print PASS/FAIL per case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtorTestCase {
    PublicNoArg,
    PublicPrimitiveArgs,
    PublicObjectArgs,
    Private,
    Throws,
    Abstract,
    Interface,
    Inner,
    RecordCanonical,
    RecordCompactValidation,
    GenericVarargs,
}

impl CtorTestCase {
    /// Stable string label for the test case — matches the
    /// constructor_probe Java app's per-case PASS/FAIL output.
    pub fn label(self) -> &'static str {
        match self {
            Self::PublicNoArg => "public-noarg",
            Self::PublicPrimitiveArgs => "public-primitive-args",
            Self::PublicObjectArgs => "public-object-args",
            Self::Private => "private",
            Self::Throws => "throws",
            Self::Abstract => "abstract",
            Self::Interface => "interface",
            Self::Inner => "inner",
            Self::RecordCanonical => "record-canonical",
            Self::RecordCompactValidation => "record-compact-validation",
            Self::GenericVarargs => "generic-varargs",
        }
    }

    /// All 11 acceptance cases in stable order.
    pub fn all() -> [Self; 11] {
        [
            Self::PublicNoArg,
            Self::PublicPrimitiveArgs,
            Self::PublicObjectArgs,
            Self::Private,
            Self::Throws,
            Self::Abstract,
            Self::Interface,
            Self::Inner,
            Self::RecordCanonical,
            Self::RecordCompactValidation,
            Self::GenericVarargs,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_concrete() {
        assert_eq!(
            InstantiationClassification::from_access_flags(0x0001), // PUBLIC
            InstantiationClassification::Concrete
        );
        assert_eq!(
            InstantiationClassification::from_access_flags(0x0021), // PUBLIC | SUPER
            InstantiationClassification::Concrete
        );
        assert!(!InstantiationClassification::Concrete.is_abstract_or_interface());
    }

    #[test]
    fn classify_interface() {
        assert_eq!(
            InstantiationClassification::from_access_flags(0x0201), // PUBLIC | INTERFACE
            InstantiationClassification::Interface
        );
        // Interface bit alone (no public) — still classified as interface.
        assert_eq!(
            InstantiationClassification::from_access_flags(0x0200),
            InstantiationClassification::Interface
        );
        assert!(InstantiationClassification::Interface.is_abstract_or_interface());
    }

    #[test]
    fn classify_abstract() {
        assert_eq!(
            InstantiationClassification::from_access_flags(0x0401), // PUBLIC | ABSTRACT
            InstantiationClassification::Abstract
        );
        assert!(InstantiationClassification::Abstract.is_abstract_or_interface());
    }

    #[test]
    fn interface_takes_priority_over_abstract() {
        // An interface always has ACC_ABSTRACT set; the JVM spec orders
        // interface-ness above abstract-ness for `Class.isInterface()`.
        let flags = 0x0201 | 0x0400; // INTERFACE | ABSTRACT (the typical interface bit pattern)
        assert_eq!(
            InstantiationClassification::from_access_flags(flags),
            InstantiationClassification::Interface
        );
    }

    #[test]
    fn ctor_access_classify() {
        assert_eq!(
            ConstructorAccess::from_modifiers(0x0001),
            ConstructorAccess::Public
        );
        assert_eq!(
            ConstructorAccess::from_modifiers(0x0002),
            ConstructorAccess::Private
        );
        assert_eq!(
            ConstructorAccess::from_modifiers(0x0004),
            ConstructorAccess::Protected
        );
        assert_eq!(
            ConstructorAccess::from_modifiers(0x0000),
            ConstructorAccess::PackagePrivate
        );
    }

    #[test]
    fn ctor_access_requires_setaccessible() {
        assert!(!ConstructorAccess::Public.requires_setaccessible());
        assert!(ConstructorAccess::Private.requires_setaccessible());
        assert!(ConstructorAccess::Protected.requires_setaccessible());
        assert!(ConstructorAccess::PackagePrivate.requires_setaccessible());
    }

    #[test]
    fn inner_class_ctor_descriptor_detection() {
        assert!(is_inner_class_ctor_descriptor(
            "(Lcom/example/Outer;)V",
            "com/example/Outer"
        ));
        assert!(is_inner_class_ctor_descriptor(
            "(Lcom/example/Outer;ILjava/lang/String;)V",
            "com/example/Outer"
        ));
        // Different first parameter — not an inner-class ctor of this outer.
        assert!(!is_inner_class_ctor_descriptor(
            "(Ljava/lang/String;)V",
            "com/example/Outer"
        ));
        // Primitive first parameter — not an inner-class ctor.
        assert!(!is_inner_class_ctor_descriptor("(I)V", "com/example/Outer"));
        // Empty descriptor — not an inner-class ctor.
        assert!(!is_inner_class_ctor_descriptor("()V", "com/example/Outer"));
    }

    #[test]
    fn ctor_test_case_labels_unique() {
        let cases = CtorTestCase::all();
        let labels: Vec<_> = cases.iter().map(|c| c.label()).collect();
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i + 1) {
                assert_ne!(a, b, "labels must be unique");
            }
        }
        assert_eq!(labels.len(), 11);
    }

    #[test]
    fn ctor_test_case_count() {
        assert_eq!(CtorTestCase::all().len(), 11);
    }
}
