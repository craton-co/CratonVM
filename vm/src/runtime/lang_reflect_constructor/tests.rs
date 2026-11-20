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
