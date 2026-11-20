use super::*;
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::BasicConstraints;
use openssl::x509::{X509Name, X509};

fn serial() -> openssl::asn1::Asn1Integer {
    let mut bn = BigNum::new().expect("bn");
    bn.rand(64, MsbOption::MAYBE_ZERO, false).expect("rand");
    bn.to_asn1_integer().expect("serial")
}

fn name(cn: &str) -> X509Name {
    let mut n = X509Name::builder().expect("name builder");
    n.append_entry_by_text("CN", cn).expect("cn");
    n.build()
}

fn p384_key() -> PKey<Private> {
    let group = EcGroup::from_curve_name(Nid::SECP384R1).expect("group");
    PKey::from_ec_key(EcKey::generate(&group).expect("keygen")).expect("pkey")
}

/// A certificate for `subject`/`key`, signed by `(issuer_name, issuer_key)`,
/// CA or leaf.
fn cert(
    subject: &str,
    key: &PKey<Private>,
    issuer: &str,
    issuer_key: &PKey<Private>,
    ca: bool,
) -> Vec<u8> {
    let mut b = X509::builder().expect("builder");
    b.set_version(2).expect("v3");
    b.set_serial_number(&serial()).expect("serial");
    b.set_subject_name(&name(subject)).expect("subject");
    b.set_issuer_name(&name(issuer)).expect("issuer");
    b.set_pubkey(key).expect("pubkey");
    b.set_not_before(&Asn1Time::days_from_now(0).expect("nb"))
        .expect("nb");
    b.set_not_after(&Asn1Time::days_from_now(3650).expect("na"))
        .expect("na");
    let bc = if ca {
        BasicConstraints::new().critical().ca().build()
    } else {
        BasicConstraints::new().critical().build()
    };
    b.append_extension(bc.expect("bc")).expect("bc ext");
    b.sign(issuer_key, MessageDigest::sha384()).expect("sign");
    b.build().to_der().expect("der")
}

fn trust_with(anchor_der: Vec<u8>) -> TrustManagerState {
    let mut trust = TrustManagerState::default();
    insert_anchor(&mut trust, anchor_der);
    trust
}

/// A peer that ends its chain with a CROSS-SIGNED copy of a root that IS in
/// the trust store — `www.cloudflare.com` and `adoptium.net` both serve
/// `CN=GTS Root R4` as signed by `CN=GlobalSign Root CA`, whose bytes
/// differ from the self-signed GTS Root R4 in JDK 25's cacerts and whose
/// own issuer that cacerts no longer ships.
///
/// What carries it is the PATH REBUILD, not the anchor comparison: the
/// cross-signed tail is dropped by `select_path` and the anchor is found
/// by issuer lookup. Named that way because the first version of this test
/// was written to guard a relaxation of `presented_cert_is_anchor` and
/// passed just as well with that relaxation reverted — it never touched
/// it. Breaking `rebuild_path` is what fails this.
#[test]
fn a_cross_signed_tail_is_dropped_and_the_real_anchor_still_found() {
    let root_key = p384_key();
    let other_root_key = p384_key();
    let leaf_key = p384_key();

    let root_self_signed = cert("Trusted Root", &root_key, "Trusted Root", &root_key, true);
    // The SAME subject and the SAME key, certified by somebody else — the
    // shape a real cross-certificate has.
    let root_cross_signed = cert(
        "Trusted Root",
        &root_key,
        "Other Root",
        &other_root_key,
        true,
    );
    assert_ne!(
        root_self_signed, root_cross_signed,
        "the fixture must present a DIFFERENT encoding, or it proves nothing"
    );
    let leaf = cert("leaf.example", &leaf_key, "Trusted Root", &root_key, false);

    let trust = trust_with(root_self_signed);
    // Only the cross-signed copy is on the wire; the issuer that signed it
    // is NOT in the trust store, exactly as with GlobalSign Root CA.
    let chain = vec![leaf, root_cross_signed];
    assert!(
        validate_chain(&chain, &trust).is_ok(),
        "the cross-signed tail must be dropped and the real anchor still found"
    );
}

/// …and the same subject with a DIFFERENT key must still be refused, or
/// the relaxation above would be a hole rather than a fix.
#[test]
fn the_same_subject_with_a_different_key_is_not_that_root() {
    let root_key = p384_key();
    let impostor_key = p384_key();
    let other_root_key = p384_key();
    let leaf_key = p384_key();

    let root_self_signed = cert("Trusted Root", &root_key, "Trusted Root", &root_key, true);
    let impostor = cert(
        "Trusted Root",
        &impostor_key,
        "Other Root",
        &other_root_key,
        true,
    );
    let leaf = cert(
        "leaf.example",
        &leaf_key,
        "Trusted Root",
        &impostor_key,
        false,
    );

    let trust = trust_with(root_self_signed);
    assert!(
        validate_chain(&vec![leaf, impostor], &trust).is_err(),
        "a certificate that only borrows the anchor's NAME must be refused"
    );
}

/// The whole chain signed by P-384 keys. Before the named-curve verifier
/// this failed with `BadSignature` at whichever index first had a P-384
/// issuer — six of twenty live public sites.
#[test]
fn a_p384_chain_validates() {
    let root_key = p384_key();
    let inter_key = p384_key();
    let leaf_key = p384_key();
    let root = cert("P384 Root", &root_key, "P384 Root", &root_key, true);
    let inter = cert(
        "P384 Intermediate",
        &inter_key,
        "P384 Root",
        &root_key,
        true,
    );
    let leaf = cert(
        "leaf.example",
        &leaf_key,
        "P384 Intermediate",
        &inter_key,
        false,
    );

    let trust = trust_with(root);
    assert!(
        validate_chain(&vec![leaf.clone(), inter.clone()], &trust).is_ok(),
        "a P-384 chain to a P-384 anchor must validate"
    );

    // The paired refusal: one flipped byte in the leaf's signature must
    // make it fail, or "validates" above would also hold for a verifier
    // that never checks anything.
    let mut tampered = leaf;
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert!(
        validate_chain(&vec![tampered, inter], &trust).is_err(),
        "a tampered P-384 signature must be refused"
    );
}
