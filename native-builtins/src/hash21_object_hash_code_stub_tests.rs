use super::reflect_stub_hash_is_structural;
use crate::class_facts_memo::NameFacts;

#[test]
fn hash21_only_an_instance_of_a_stub_class_hashes_structurally() {
    for stub in [
        "java/lang/reflect/GenericArrayType",
        "java/lang/reflect/ParameterizedType",
        "java/lang/reflect/WildcardType",
    ] {
        let facts = Some(NameFacts::from_name(stub));
        assert!(reflect_stub_hash_is_structural(false, facts), "{stub} instance");
        assert!(
            !reflect_stub_hash_is_structural(true, facts),
            "{stub}[] is an array: identity hash"
        );
    }
    for other in ["java/lang/Object", "java/lang/String", "java/lang/reflect/Type"] {
        let facts = Some(NameFacts::from_name(other));
        assert!(!reflect_stub_hash_is_structural(false, facts), "{other}");
        assert!(!reflect_stub_hash_is_structural(true, facts), "{other}[]");
    }
    assert!(!reflect_stub_hash_is_structural(false, None), "unknown class id");
    assert!(!reflect_stub_hash_is_structural(true, None), "unknown class id, array");
}

/// Both entry points (the leaf and the full funnel) ask the one helper with
/// the receiver's array-ness: a source-shape pin, as the neighbouring tests
/// of this file do for paths a mock cannot run (its arrays report class 0).
#[test]
fn hash21_the_leaf_and_the_funnel_both_ask_the_array_aware_helper() {
    let src = include_str!("lib_parts/arrays_range_or_throw.rs").replace("\r\n", "\n");
    for entry in [
        "fn native_object_hash_code(ctx: &mut dyn NativeContext",
        "fn object_hash_code_fast(ctx: &dyn NativeContext",
    ] {
        let start = src.find(entry).expect("the entry point");
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("end of entry point")];
        let helper = body
            .find("reflect_stub_hash_is_structural(")
            .expect("asks the helper");
        let array = body
            .find("ctx.object_is_array(this)")
            .expect("passes the array flag");
        assert!(helper < array, "{entry}: the array flag is the helper's argument");
        assert!(
            !body.contains("NameFacts::is_reflect_type_stub)"),
            "{entry}: no bare stub test left"
        );
    }
    let leaf = src.find("fn native_object_hash_code_leaf(").expect("the leaf");
    let leaf = &src[leaf..];
    let leaf = &leaf[..leaf.find("\n}\n").expect("end of leaf")];
    assert!(leaf.contains("object_hash_code_fast(&*ctx, *this)"));
}
