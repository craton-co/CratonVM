use super::*;
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::ssl::{SslAcceptor, SslMethod};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509Name, X509};

/// A self-signed CA of `bits` bits, and a leaf for `localhost` signed by
/// it. `bits` is a parameter because the SECOND thing this connector
/// changed -- the certificate security level -- is only visible at a key
/// size OpenSSL's default level 2 refuses and the JDK's floor allows.
fn ca_and_leaf(bits: u32) -> ((X509, PKey<Private>), (X509, PKey<Private>)) {
    let mk_key = || PKey::from_rsa(Rsa::generate(bits).expect("rsa")).expect("pkey");
    let mk_name = |cn: &str| {
        let mut n = X509Name::builder().expect("name builder");
        n.append_entry_by_text("CN", cn).expect("cn");
        n.build()
    };
    let serial = || {
        let mut bn = BigNum::new().expect("bn");
        bn.rand(64, MsbOption::MAYBE_ZERO, false).expect("rand");
        bn.to_asn1_integer().expect("serial")
    };
    let not_before = Asn1Time::days_from_now(0).expect("nb");
    let not_after = Asn1Time::days_from_now(3650).expect("na");

    let ca_key = mk_key();
    let ca_name = mk_name("CratonVM Chain Test CA");
    let mut b = X509::builder().expect("ca builder");
    b.set_version(2).expect("v3");
    b.set_serial_number(&serial()).expect("serial");
    b.set_subject_name(&ca_name).expect("subject");
    b.set_issuer_name(&ca_name).expect("issuer");
    b.set_pubkey(&ca_key).expect("pubkey");
    b.set_not_before(&not_before).expect("nb");
    b.set_not_after(&not_after).expect("na");
    b.append_extension(BasicConstraints::new().critical().ca().build().expect("bc"))
        .expect("bc ext");
    b.sign(&ca_key, MessageDigest::sha256()).expect("sign ca");
    let ca = b.build();

    let leaf_key = mk_key();
    let mut b = X509::builder().expect("leaf builder");
    b.set_version(2).expect("v3");
    b.set_serial_number(&serial()).expect("serial");
    b.set_subject_name(&mk_name("localhost")).expect("subject");
    b.set_issuer_name(ca.subject_name()).expect("issuer");
    b.set_pubkey(&leaf_key).expect("pubkey");
    b.set_not_before(&not_before).expect("nb");
    b.set_not_after(&not_after).expect("na");
    b.append_extension(BasicConstraints::new().critical().build().expect("bc"))
        .expect("bc ext");
    let ctx = b.x509v3_context(Some(&ca), None);
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .ip("127.0.0.1")
        .build(&ctx)
        .expect("san");
    b.append_extension(san).expect("san ext");
    b.sign(&ca_key, MessageDigest::sha256()).expect("sign leaf");
    let leaf = b.build();

    ((ca, ca_key), (leaf, leaf_key))
}

/// A one-connection TLS server presenting `leaf` with `ca` as an EXTRA
/// CHAIN CERT -- i.e. a real two-certificate chain on the wire, which is
/// the whole point. Returns its port and the thread handle.
fn serve_once(
    ca: X509,
    leaf: X509,
    leaf_key: PKey<Private>,
    security_level: u32,
) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        let mut b = SslAcceptor::mozilla_intermediate(SslMethod::tls()).expect("acceptor");
        b.set_security_level(security_level);
        b.set_private_key(&leaf_key).expect("key");
        b.set_certificate(&leaf).expect("cert");
        b.add_extra_chain_cert(ca).expect("chain cert");
        let acceptor = b.build();
        if let Ok((stream, _)) = listener.accept() {
            // The handshake is all that is under test. A rejected one is
            // the assertion's business, not this thread's.
            let _ = acceptor.accept(stream);
        }
    });
    (port, handle)
}

/// The defect, stated as a test: an application TrustManager is handed
/// whatever this vector holds, and from ONE certificate it cannot build a
/// path to a root. 20 of 20 live public sites were rejected that way.
#[test]
fn client_captures_the_whole_chain_not_just_the_leaf() {
    let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(2048);
    let ca_der = ca.to_der().expect("ca der");
    let leaf_der = leaf.to_der().expect("leaf der");
    let (port, server) = serve_once(ca, leaf, leaf_key, 1);

    let cfg = OpensslClientConfig {
        roots: vec![ca_der.clone()],
        replace_roots: true,
        skip_verify: false,
        max_tls12: false,
    };
    let id = s2_openssl_tls_connect(&cfg, "localhost", port).unwrap_or_else(|e| {
        panic!("handshake against the two-certificate fixture failed: {e}")
    });
    let chain = s2_tls_peer_cert_chain_der(id).expect("registry entry");
    let _ = s2_tls_close(id);
    let _ = server.join();

    assert_eq!(
        chain.len(),
        2,
        "peer chain must be leaf + issuing CA; a length of 1 is the defect"
    );
    assert_eq!(chain[0], leaf_der, "the LEAF must come first (JSSE order)");
    assert_eq!(chain[1], ca_der, "the issuer must follow it");
}

/// The residue, stated as a test. A 1024-bit RSA chain is something the
/// JDK accepts (`jdk.certpath.disabledAlgorithms` draws its line AT 1024)
/// and OpenSSL at its default security level of 2 refuses outright, which
/// is what a CratonVM client with no trust store configured used to do:
///
/// ```text
/// HOTSPOT   HANDSHAKE-OK   CRATONVM  REFUSED (EE certificate key too weak)
/// ```
///
/// The server is pinned to level 0 so that only the CLIENT's level is
/// under test -- otherwise a refusal could be the fixture's own.
#[test]
fn client_security_level_matches_the_jdks_1024_bit_floor() {
    let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(1024);
    let ca_der = ca.to_der().expect("ca der");
    let (port, server) = serve_once(ca, leaf, leaf_key, 0);

    let cfg = OpensslClientConfig {
        roots: vec![ca_der],
        replace_roots: true,
        skip_verify: false,
        max_tls12: false,
    };
    let result = s2_openssl_tls_connect(&cfg, "localhost", port);
    let verdict = match &result {
        Ok(id) => {
            let chain = s2_tls_peer_cert_chain_der(*id).unwrap_or_default();
            let _ = s2_tls_close(*id);
            Ok(chain.len())
        }
        Err(e) => Err(e.to_string()),
    };
    let _ = server.join();
    assert_eq!(
        verdict,
        Ok(2),
        "a 1024-bit chain the JDK accepts must not be refused by the \
         client's security level (see CLIENT_SECURITY_LEVEL)"
    );
}

/// Fail CLOSED is not weakened by any of the above: a chain that does NOT
/// reach the configured anchor is still refused. Without this, a test that
/// only ever asserts acceptance passes just as well against a connector
/// that verifies nothing at all.
#[test]
fn client_still_refuses_a_chain_that_reaches_no_configured_anchor() {
    let ((ca, _ca_key), (leaf, leaf_key)) = ca_and_leaf(2048);
    // The anchor handed to the client is an UNRELATED CA, so the peer's
    // chain is well-formed and simply does not reach it.
    let ((other_ca, _other_key), (_l, _k)) = ca_and_leaf(2048);
    let other_der = other_ca.to_der().expect("der");
    let (port, server) = serve_once(ca, leaf, leaf_key, 1);

    let cfg = OpensslClientConfig {
        roots: vec![other_der],
        replace_roots: true,
        skip_verify: false,
        max_tls12: false,
    };
    let result = s2_openssl_tls_connect(&cfg, "localhost", port);
    let _ = server.join();
    match result {
        Ok(id) => {
            let _ = s2_tls_close(id);
            panic!("a chain reaching no configured anchor must be refused");
        }
        Err(TlsConnectFailure::Handshake(_)) => {}
        Err(TlsConnectFailure::Tcp(e)) => {
            panic!("expected a handshake rejection, got a TCP failure: {e}")
        }
    }
}
