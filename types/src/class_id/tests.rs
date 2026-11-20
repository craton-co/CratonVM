use super::*;
use std::collections::HashSet;

// -- ClassId tests --

#[test]
fn class_id_new_and_as_u32() {
    let id = ClassId::new(42);
    assert_eq!(id.as_u32(), 42);
}

#[test]
fn class_id_zero() {
    let id = ClassId::new(0);
    assert_eq!(id.as_u32(), 0);
}

#[test]
fn class_id_max() {
    let id = ClassId::new(u32::MAX);
    assert_eq!(id.as_u32(), u32::MAX);
}

#[test]
fn class_id_equality() {
    let a = ClassId::new(10);
    let b = ClassId::new(10);
    let c = ClassId::new(11);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn class_id_copy_semantics() {
    let a = ClassId::new(7);
    let b = a; // Copy
    assert_eq!(a, b); // a is still valid after copy
}

#[test]
fn class_id_hash_consistency() {
    // Equal ClassIds must hash to the same value
    let mut set = HashSet::new();
    set.insert(ClassId::new(100));
    assert!(set.contains(&ClassId::new(100)));
    assert!(!set.contains(&ClassId::new(101)));
}

#[test]
fn class_id_hash_uniqueness() {
    // Different ClassIds should generally produce different hashes
    let mut set = HashSet::new();
    for i in 0..1000 {
        set.insert(ClassId::new(i));
    }
    assert_eq!(set.len(), 1000);
}

#[test]
fn class_id_display() {
    assert_eq!(format!("{}", ClassId::new(0)), "0");
    assert_eq!(format!("{}", ClassId::new(42)), "42");
    assert_eq!(
        format!("{}", ClassId::new(u32::MAX)),
        format!("{}", u32::MAX)
    );
}

#[test]
fn class_id_debug() {
    let id = ClassId::new(5);
    let debug = format!("{:?}", id);
    assert!(debug.contains("ClassId"));
    assert!(debug.contains("5"));
}

#[test]
fn class_id_const_new() {
    // Verify const fn works in const context
    const ID: ClassId = ClassId::new(999);
    assert_eq!(ID.as_u32(), 999);
}

// -- ClassLoaderId tests --

#[test]
fn class_loader_id_display() {
    assert_eq!(format!("{}", ClassLoaderId::Bootstrap), "bootstrap");
    assert_eq!(format!("{}", ClassLoaderId::Extension), "extension");
    assert_eq!(format!("{}", ClassLoaderId::Application), "application");
}

#[test]
fn class_loader_id_equality() {
    assert_eq!(ClassLoaderId::Bootstrap, ClassLoaderId::Bootstrap);
    assert_ne!(ClassLoaderId::Bootstrap, ClassLoaderId::Extension);
    assert_ne!(ClassLoaderId::Extension, ClassLoaderId::Application);
}

#[test]
fn class_loader_id_hash() {
    let mut set = HashSet::new();
    set.insert(ClassLoaderId::Bootstrap);
    set.insert(ClassLoaderId::Extension);
    set.insert(ClassLoaderId::Application);
    assert_eq!(set.len(), 3);
    assert!(set.contains(&ClassLoaderId::Bootstrap));
}

#[test]
fn class_loader_id_clone() {
    let a = ClassLoaderId::Bootstrap;
    let b = a.clone();
    assert_eq!(a, b);
}

// -- native-API `u32` codec ------------------------------------------
//
// Regression cover for the CGLIB `@Configuration` singleton bug: the
// encode (`loader_id_of_class`) and the decode (`define_class_full` and
// friends) were hand-written at six separate call sites and drifted, so
// `Application` round-tripped to `UserDefined(2)` and every enhanced
// subclass landed in a different runtime package than its superclass.

#[test]
fn native_id_encoding_matches_the_documented_table() {
    assert_eq!(ClassLoaderId::Bootstrap.to_native_id(), 0);
    assert_eq!(ClassLoaderId::Extension.to_native_id(), 1);
    assert_eq!(ClassLoaderId::Application.to_native_id(), 2);
    assert_eq!(ClassLoaderId::UserDefined(3).to_native_id(), 3);
    assert_eq!(ClassLoaderId::UserDefined(9999).to_native_id(), 9999);
}

#[test]
fn from_native_id_is_the_exact_inverse_of_to_native_id() {
    let mut all = vec![
        ClassLoaderId::Bootstrap,
        ClassLoaderId::Extension,
        ClassLoaderId::Application,
    ];
    // Only ids >= 3 are legal user-defined namespaces (see
    // `NATIVE_FIRST_USER_DEFINED` / `allocate_loader_id`).
    all.extend((ClassLoaderId::NATIVE_FIRST_USER_DEFINED..64).map(ClassLoaderId::UserDefined));
    all.push(ClassLoaderId::UserDefined(u32::MAX));
    for id in all {
        assert_eq!(
            ClassLoaderId::from_native_id(id.to_native_id()),
            id,
            "round-trip must be lossless for {id}"
        );
    }
}

#[test]
fn default_sentinel_decode_differs_from_strict_decode_only_at_zero() {
    assert_eq!(
        ClassLoaderId::from_native_id(0),
        ClassLoaderId::Bootstrap,
        "strict decode: 0 is the bootstrap loader"
    );
    assert_eq!(
        ClassLoaderId::from_native_id_or_default(0),
        ClassLoaderId::Application,
        "sentinel decode: 0 means 'unspecified -> application/global namespace'"
    );
    for id in 1..64u32 {
        assert_eq!(
            ClassLoaderId::from_native_id_or_default(id),
            ClassLoaderId::from_native_id(id),
            "the two decoders may only disagree at 0 (id {id})"
        );
    }
}

#[test]
fn application_never_decodes_to_a_user_defined_namespace() {
    // The exact shape of the fixed bug: `Application` encodes to 2, and 2
    // must NOT come back as `UserDefined(2)` under either decoder.
    let encoded = ClassLoaderId::Application.to_native_id();
    assert_eq!(
        ClassLoaderId::from_native_id(encoded),
        ClassLoaderId::Application
    );
    assert_eq!(
        ClassLoaderId::from_native_id_or_default(encoded),
        ClassLoaderId::Application
    );
    assert_eq!(ClassLoaderId::from_native_id(1), ClassLoaderId::Extension);
}

#[test]
fn reserved_ids_are_below_the_first_user_defined_namespace() {
    assert!(ClassLoaderId::NATIVE_BOOTSTRAP < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
    assert!(ClassLoaderId::NATIVE_EXTENSION < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
    assert!(ClassLoaderId::NATIVE_APPLICATION < ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
    // Every id at or above the boundary decodes to a user-defined
    // namespace under both decoders.
    for id in ClassLoaderId::NATIVE_FIRST_USER_DEFINED..16 {
        assert_eq!(
            ClassLoaderId::from_native_id(id),
            ClassLoaderId::UserDefined(id)
        );
        assert_eq!(
            ClassLoaderId::from_native_id_or_default(id),
            ClassLoaderId::UserDefined(id)
        );
    }
}

#[test]
fn is_builtin_matches_the_reserved_id_range() {
    assert!(ClassLoaderId::Bootstrap.is_builtin());
    assert!(ClassLoaderId::Extension.is_builtin());
    assert!(ClassLoaderId::Application.is_builtin());
    assert!(!ClassLoaderId::UserDefined(3).is_builtin());
    assert!(!ClassLoaderId::UserDefined(u32::MAX).is_builtin());
}
