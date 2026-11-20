use super::static_receiver_admits_arrays;

/// A class-id guard on an array receiver compares the COMPONENT's id, so
/// every owner an array can be the receiver of must ask for the kind
/// screen: the three array supertypes, an unknown owner, and an array
/// owner (`invokevirtual [I.clone()`).
#[test]
fn array_owners_ask_for_the_kind_screen() {
    for owner in [
        "",
        "java/lang/Object",
        "java/lang/Cloneable",
        "java/io/Serializable",
        "[I",
        "[Ljava/lang/String;",
        "[[J",
    ] {
        assert!(static_receiver_admits_arrays(owner), "{owner}");
    }
    for owner in ["java/lang/String", "java/util/ArrayList", "Foo"] {
        assert!(!static_receiver_admits_arrays(owner), "{owner}");
    }
}

