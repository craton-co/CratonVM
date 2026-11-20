use super::ADVERTISED_KEYSTORE_CLASSES;

/// A `KeyStore` row the provider table advertises must resolve to a class
/// this module actually serves.
///
/// The two are joined only by a string, and dispatch here is by CLASS NAME
/// with no inheritance walk — so correcting a row to the real JDK class
/// name, which is exactly the right thing to do, silently unregisters the
/// engine surface. That happened: SUN's `KeyStore.PKCS12` was corrected
/// from `PKCS12KeyStore` to `PKCS12KeyStore$DualFormatPKCS12`, the JKS
/// twin of the same change WAS registered, and the PKCS12 one was not.
/// `KeyStore.getInstance("PKCS12")` — the default — then loaded nothing,
/// and netty's `JdkSslEngineTest` went from 754 passing to 563.
///
/// Nothing in the build said so. The provider table was right, the
/// registration list was right for what it listed, and the gap was between
/// them. This test is that gap.
#[test]
fn every_advertised_keystore_class_has_an_engine_surface() {
    let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
    super::register_keystore_real(&mut registry);
    let mut missing = Vec::new();
    for fqn in ADVERTISED_KEYSTORE_CLASSES {
        // `engineLoad` stands for the whole surface: `register_engine_surface`
        // registers them together or not at all.
        if registry
            .find(fqn, "engineLoad", "(Ljava/io/InputStream;[C)V")
            .is_none()
        {
            missing.push(*fqn);
        }
    }
    assert!(
        missing.is_empty(),
        "the provider table advertises these KeyStore classes and this module \
         registers no engine surface for them, so every engineLoad on one is a \
         method with no body: {missing:?}"
    );
}

/// …and the list the guard walks must be the list the provider table
/// actually publishes, or the guard checks a fiction. Compared against the
/// SOURCE of `provider_chain`'s SUN/SunJSSE rows rather than a copy.
#[test]
fn the_advertised_list_matches_the_provider_table() {
    let src = include_str!("../jca/provider_chain.rs");
    for fqn in ADVERTISED_KEYSTORE_CLASSES {
        let dotted = fqn.replace('/', ".");
        assert!(
            src.contains(&format!("\"{dotted}\"")),
            "{dotted} is in ADVERTISED_KEYSTORE_CLASSES but no longer appears in \
             provider_chain.rs — drop it here, or the guard is checking a class \
             nothing advertises"
        );
    }
}
