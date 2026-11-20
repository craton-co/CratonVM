use super::canonical_concrete_for_interface;

/// The six names Path B substitutes for, pinned. If a lane adds a seventh,
/// this test is where it has to say so — and the array refusal at the call
/// site picks it up automatically, because it reads the same function.
#[test]
fn the_substituted_interfaces_are_exactly_these_six() {
    let mapped: Vec<(&str, &str)> = [
        "java/util/Set",
        "java/util/Collection",
        "java/lang/Iterable",
        "java/util/List",
        "java/util/Map",
        "java/util/Iterator",
    ]
    .into_iter()
    .map(|i| (i, canonical_concrete_for_interface(i)))
    .collect();
    assert_eq!(
        mapped,
        vec![
            ("java/util/Set", "java/util/HashSet"),
            ("java/util/Collection", "java/util/HashSet"),
            ("java/lang/Iterable", "java/util/ArrayList"),
            ("java/util/List", "java/util/ArrayList"),
            ("java/util/Map", "java/util/HashMap"),
            ("java/util/Iterator", "java/util/HashMap$KeyItr"),
        ]
    );
}

/// The load-bearing absence. An array type implements `Cloneable` and
/// `java.io.Serializable` and nothing else (JLS 4.10.3). If either ever
/// gained a canonical mapping, "canonical is non-empty AND receiver is an
/// array" would stop being a proof of `IncompatibleClassChangeError` — so
/// the refusal's soundness is asserted here, not just commented.
#[test]
fn the_two_interfaces_an_array_really_implements_are_never_substituted() {
    assert_eq!(canonical_concrete_for_interface("java/lang/Cloneable"), "");
    assert_eq!(canonical_concrete_for_interface("java/io/Serializable"), "");
}

/// The refusal must not swallow ordinary `Object` methods on an array.
/// `clone`/`hashCode`/`getClass` on an `Object[]` are legal and reach the
/// no-`Code` arm through `java/lang/Object`, which maps to nothing — so
/// the array check cannot fire for them.
#[test]
fn object_and_unrelated_types_are_not_substituted() {
    for name in [
        "java/lang/Object",
        "java/util/Map$Entry",
        "java/util/SequencedCollection",
        "java/net/http/HttpRequest",
        "java/util/stream/Stream",
        "",
    ] {
        assert_eq!(
            canonical_concrete_for_interface(name),
            "",
            "{name} must not be substituted"
        );
    }
}

/// The exact triple this record was written about:
/// `java/util/Map.isEmpty()Z` on an `Object[]` receiver. `java/util/Map`
/// is substituted, so an array receiver at that site is refused.
///
/// MEASURED counterpart: `[CANONICAL] java/util/Map java/util/HashMap
/// isEmpty 1` — the only row the whole 105-class corpus produces, in
/// either policy mode.
#[test]
fn the_map_is_empty_triple_is_the_substituted_one() {
    assert_eq!(
        canonical_concrete_for_interface("java/util/Map"),
        "java/util/HashMap"
    );
    assert!(!canonical_concrete_for_interface("java/util/Map").is_empty());
}
