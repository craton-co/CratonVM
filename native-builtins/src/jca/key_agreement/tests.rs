use super::*;

/// Every algorithm this engine serves names the SPI class and the provider
/// HotSpot own registration does — enumerated from `Provider.getServices()`
/// on JDK 25, where the SunEC/SunJCE split is not derivable from the family.
#[test]
fn ka_spi_classes_match_the_jdk_registrations() {
    for (alg, spi, provider) in [
        ("ECDH", ECDH_SPI, "SunEC"),
        ("ecdh", ECDH_SPI, "SunEC"),
        ("X25519", "sun/security/ec/XDHKeyAgreement$X25519", "SunEC"),
        ("X448", "sun/security/ec/XDHKeyAgreement$X448", "SunEC"),
        ("XDH", "sun/security/ec/XDHKeyAgreement", "SunEC"),
        ("DH", "com/sun/crypto/provider/DHKeyAgreement", "SunJCE"),
        (
            "DiffieHellman",
            "com/sun/crypto/provider/DHKeyAgreement",
            "SunJCE",
        ),
    ] {
        assert_eq!(ka_spi_class(alg), Some((spi, provider)), "{alg}");
    }
    assert_eq!(ka_spi_class("TOTALLY-BOGUS-ALG"), None);
    assert_eq!(ka_spi_class(""), None);
}
