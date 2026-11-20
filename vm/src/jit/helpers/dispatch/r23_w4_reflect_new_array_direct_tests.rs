use super::reflect_new_array_direct_admits_name as admits;

/// Reference components only; the native keeps primitives, `void` and the
/// 255-dimension `IllegalArgumentException`.
#[test]
fn admits_reference_components_only() {
    assert!(admits("java/lang/String"));
    assert!(admits("Foo"));
    assert!(admits("[Ljava/lang/String;"));
    assert!(admits("[I"));
    assert!(admits(&format!("{}I", "[".repeat(254))));
    assert!(!admits(&format!("{}I", "[".repeat(255))));
    for p in [
        "", "void", "V", "int", "long", "float", "double", "boolean", "byte", "char", "short",
    ] {
        assert!(!admits(p), "{p}");
    }
}
