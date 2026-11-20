use super::*;

// The boundary itself, with both halves pinned. A test that only asserted
// the accept side would be satisfied by `spi_is_adopted(_) = true`.
#[test]
fn the_adopted_boundary_is_sun_security_ssl_and_nothing_else() {
    for ours in [
        "sun/security/ssl/SSLContextImpl",
        "sun/security/ssl/SSLContextImpl$TLSContext",
        "sun/security/ssl/SSLContextImpl$TLS13Context",
        "sun/security/ssl/SSLContextImpl$DTLS12Context",
    ] {
        assert!(spi_is_adopted(ours), "{ours} must be CratonVM's own SPI");
    }
    for theirs in [
        "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
        "org/conscrypt/OpenSSLContextImpl",
        "io/netty/handler/ssl/JdkSslContext",
        // Near-misses that must NOT be swept in by a sloppy prefix.
        "sun/security/sslx/Impostor",
        "com/example/sun/security/ssl/Impostor",
        "",
    ] {
        assert!(
            !spi_is_adopted(theirs),
            "{theirs} is somebody else's SPI and must be delegated to"
        );
    }
}

// The prefix is a package, not a class: SunJSSE's per-protocol contexts
// are nested subclasses and every one of them is ours.
#[test]
fn the_adopted_list_is_a_package_prefix() {
    assert_eq!(ADOPTED_SPI_PACKAGES, &["sun/security/ssl/"]);
    assert!(spi_is_adopted("sun/security/ssl/anything/at/all"));
}
