use super::*;

#[test]
fn every_triple_has_a_handler_and_is_an_intrinsic() {
    assert_eq!(JDK_CRYPTO_INTRINSIC_TRIPLES.len(), HANDLERS.len());
    let mut r = NativeMethodRegistry::new();
    let leaf_before = r.current_leaf();
    register_jdk_crypto_intrinsics(&mut r);
    assert_eq!(r.current_leaf(), leaf_before, "leaf claim restored");
    for (c, m, d) in JDK_CRYPTO_INTRINSIC_TRIPLES {
        assert_eq!(
            r.kind_of(c, m, d),
            Some(NativeKind::Intrinsic),
            "{c}.{m}{d} must be a reviewed Intrinsic"
        );
    }
}
