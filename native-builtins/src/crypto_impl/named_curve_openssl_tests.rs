use super::*;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::PKey;
use openssl::sign::Signer;

fn round_trip(nid: Nid, digest: MessageDigest, expect: &str) {
    let group = EcGroup::from_curve_name(nid).expect("group");
    let key = EcKey::generate(&group).expect("keygen");
    let spki = key.public_key_to_der().expect("spki der");
    let pkey = PKey::from_ec_key(key).expect("pkey");

    let msg = b"cratonvm named-curve verification vector";
    let mut signer = Signer::new(digest, &pkey).expect("signer");
    signer.update(msg).expect("update");
    let sig = signer.sign_to_vec().expect("sign");

    let parsed = parse_named_ec_public_key(&spki)
        .unwrap_or_else(|| panic!("{expect}: OpenSSL's own SPKI did not parse"));
    assert_eq!(parsed.curve.name, expect, "curve read from the SPKI");

    let hash = |m: &[u8]| -> Vec<u8> {
        match digest.type_() {
            t if t == MessageDigest::sha256().type_() => Sha256::digest(m).to_vec(),
            t if t == MessageDigest::sha384().type_() => Sha384::digest(m).to_vec(),
            _ => Sha512::digest(m).to_vec(),
        }
    };

    assert!(
        verify_named_ecdsa(&parsed, &hash(msg), &sig),
        "{expect}: a signature OpenSSL made was rejected"
    );
    assert!(
        !verify_named_ecdsa(
            &parsed,
            &hash(b"cratonvm named-curve verification vecto!"),
            &sig
        ),
        "{expect}: the same signature was accepted over a DIFFERENT message"
    );
}

#[test]
fn p256_sha256_round_trips_against_openssl() {
    round_trip(Nid::X9_62_PRIME256V1, MessageDigest::sha256(), "P-256");
}

/// The one that was broken. Every ECDSA chain on the public internet whose
/// issuer key is P-384 — Let's Encrypt Root YE, Sectigo Root E46, DigiCert
/// Global G3 TLS ECC, Google GTS Root R4 — failed here.
#[test]
fn p384_sha384_round_trips_against_openssl() {
    round_trip(Nid::SECP384R1, MessageDigest::sha384(), "P-384");
}

#[test]
fn p521_sha512_round_trips_against_openssl() {
    round_trip(Nid::SECP521R1, MessageDigest::sha512(), "P-521");
}

/// A digest WIDER than the curve order has to be truncated to the order's
/// bit length (FIPS 186-4 §6.4), not reduced modulo it. Getting that wrong
/// is invisible whenever the two happen to agree, so it is asked
/// explicitly: SHA-512 over P-256 is the widest mismatch available.
#[test]
fn a_digest_wider_than_the_order_is_truncated_not_reduced() {
    round_trip(Nid::X9_62_PRIME256V1, MessageDigest::sha512(), "P-256");
}

/// …and a digest NARROWER than the order is used whole.
#[test]
fn a_digest_narrower_than_the_order_is_used_whole() {
    round_trip(Nid::SECP521R1, MessageDigest::sha256(), "P-521");
}
