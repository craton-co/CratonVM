// The `bool` wrapper is deprecated (see its doc comment) but must keep its
// fail-closed test: it is still compiled, and the property that an `Err`
// can never surface as `true` is exactly what makes leaving it in place
// safe rather than merely convenient.
#![allow(deprecated)]

use super::*;
use crate::failure::{INVALID_KEY_EXCEPTION, SIGNATURE_EXCEPTION};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use getrandom::SysRng;
use rsa::pkcs1v15::SigningKey;
use rsa::sha2::{Sha256 as RsaSha256, Sha384 as RsaSha384, Sha512 as RsaSha512};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use sha1_next::Sha1 as RsaSha1;

/// One 1024-bit key + a SHA-256 signature over `msg`, reused by the cases
/// below (RSA keygen is the expensive part of this test module).
fn fixture(msg: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let private = rsa::PrivateKey::new(&mut SysRng, 1024).expect("test key");
    let public = private.to_public_key();
    let signer = SigningKey::<RsaSha256>::new(private);
    let sig = signer.sign(msg).to_vec();
    (
        public.n().to_be_bytes().into_vec(),
        public.e().to_be_bytes().into_vec(),
        sig,
    )
}

// ---- supported algorithm succeeds; genuine mismatch stays a `false` ----

#[test]
fn valid_and_tampered_sha256_signatures_are_distinguished() {
    let (n, e, sig) = fixture(b"shared verifier");
    assert!(verify_rsa_pkcs1_v15(
        &n,
        &e,
        DigestAlgorithm::Sha256,
        b"shared verifier",
        &sig,
    ));
    assert!(!verify_rsa_pkcs1_v15(
        &n,
        &e,
        DigestAlgorithm::Sha256,
        b"tampered",
        &sig,
    ));
}

/// The single most important property in this file: a signature that is
/// well-formed but simply does not match the message is a **legitimate
/// negative security decision**. It must stay `Ok(false)` and must NOT be
/// promoted to an exception, or every caller that legitimately expects to
/// be told "no" starts seeing errors instead.
#[test]
fn genuine_mismatch_is_ok_false_not_an_error() {
    let (n, e, sig) = fixture(b"the real message");
    assert_eq!(
        verify_rsa_pkcs1_v15_checked(
            &n,
            &e,
            DigestAlgorithm::Sha256,
            b"the real message",
            &sig
        ),
        Ok(true)
    );
    let mismatch = verify_rsa_pkcs1_v15_checked(
        &n,
        &e,
        DigestAlgorithm::Sha256,
        b"a different message",
        &sig,
    );
    assert_eq!(
        mismatch,
        Ok(false),
        "a real digest mismatch must remain a false, not become an exception"
    );
}

/// A signature of the right length whose *bits* are garbage is still a
/// genuine negative (bad padding is what a forged signature looks like),
/// not a malformed-encoding error.
#[test]
fn corrupt_signature_bits_are_ok_false() {
    let (n, e, mut sig) = fixture(b"payload");
    sig[0] ^= 0xff;
    assert_eq!(
        verify_rsa_pkcs1_v15_checked(&n, &e, DigestAlgorithm::Sha256, b"payload", &sig),
        Ok(false)
    );
}

/// Every supported digest round-trips. Guards against a future edit that
/// wires one variant to the wrong hash — which would show up as a silent
/// verification failure rather than as an error.
#[test]
fn every_supported_digest_verifies_its_own_signature() {
    let private = rsa::PrivateKey::new(&mut SysRng, 1024).expect("test key");
    let public = private.to_public_key();
    let (n, e) = (
        public.n().to_be_bytes().into_vec(),
        public.e().to_be_bytes().into_vec(),
    );
    let msg = b"multi-digest";

    let cases: Vec<(DigestAlgorithm, Vec<u8>)> = vec![
        (
            DigestAlgorithm::Sha1,
            SigningKey::<RsaSha1>::new(private.clone())
                .sign(msg)
                .to_vec(),
        ),
        (
            DigestAlgorithm::Sha256,
            SigningKey::<RsaSha256>::new(private.clone())
                .sign(msg)
                .to_vec(),
        ),
        (
            DigestAlgorithm::Sha384,
            SigningKey::<RsaSha384>::new(private.clone())
                .sign(msg)
                .to_vec(),
        ),
        (
            DigestAlgorithm::Sha512,
            SigningKey::<RsaSha512>::new(private.clone())
                .sign(msg)
                .to_vec(),
        ),
    ];
    for (alg, sig) in cases {
        assert_eq!(
            verify_rsa_pkcs1_v15_checked(&n, &e, alg, msg, &sig),
            Ok(true),
            "digest {alg:?} failed to verify its own signature"
        );
    }
}

// ---- malformed key RAISES rather than returning `false` ----

/// **The generator reproduces RFC 8017's published blobs, byte for byte.**
///
/// `pkcs1v15_digest_info_prefix` builds the `DigestInfo` DER from an OID
/// and a length instead of carrying thirteen transcribed hex constants.
/// That is only safe if it agrees with the four the RFC actually publishes
/// — these are quoted from RFC 8017 §9.2 note 1 and are the same bytes
/// `crypto_impl`'s hand-written table carried before it was deleted.
///
/// With these four pinned, the other nine follow from the same two inputs,
/// and a wrong OID is the only remaining way to get one wrong — which the
/// cross-VM signature diff in `apps/probes/W763Residuals` catches, because
/// PKCS#1 v1.5 is deterministic and HotSpot signs the same bytes.
#[test]
fn generated_prefixes_match_the_rfc8017_constants() {
    assert_eq!(
        DigestAlgorithm::Sha1.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
            0x14,
        ]
    );
    assert_eq!(
        DigestAlgorithm::Sha256.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ]
    );
    assert_eq!(
        DigestAlgorithm::Sha384.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x02, 0x05, 0x00, 0x04, 0x30,
        ]
    );
    assert_eq!(
        DigestAlgorithm::Sha512.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x03, 0x05, 0x00, 0x04, 0x40,
        ]
    );
    // MD2 and MD5, also published by the RFC, and the two whose OID arc is
    // different from every other row here.
    assert_eq!(
        DigestAlgorithm::Md2.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x02,
            0x05, 0x00, 0x04, 0x10,
        ]
    );
    assert_eq!(
        DigestAlgorithm::Md5.pkcs1v15_digest_info_prefix(),
        vec![
            0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05,
            0x05, 0x00, 0x04, 0x10,
        ]
    );
}

/// Every variant is structurally well formed and no two share a prefix.
///
/// The second half is the one that matters: two digests of the same LENGTH
/// differing only in their OID (`SHA-256` / `SHA-512/256` / `SHA3-256`, and
/// `SHA-224` / `SHA-512/224` / `SHA3-224`) are exactly where a copied arm
/// would go unnoticed — the signature would be the right size and verify
/// against nothing.
#[test]
fn every_digest_prefix_is_well_formed_and_distinct() {
    use std::collections::HashSet;
    let all = [
        DigestAlgorithm::Md2,
        DigestAlgorithm::Md5,
        DigestAlgorithm::Sha1,
        DigestAlgorithm::Sha224,
        DigestAlgorithm::Sha256,
        DigestAlgorithm::Sha384,
        DigestAlgorithm::Sha512,
        DigestAlgorithm::Sha512_224,
        DigestAlgorithm::Sha512_256,
        DigestAlgorithm::Sha3_224,
        DigestAlgorithm::Sha3_256,
        DigestAlgorithm::Sha3_384,
        DigestAlgorithm::Sha3_512,
    ];
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for d in all {
        let p = d.pkcs1v15_digest_info_prefix();
        // Outer SEQUENCE covers everything after its own tag+length, plus
        // the hash bytes that follow the prefix.
        assert_eq!(p[0], 0x30, "{d:?}");
        assert_eq!(
            p[1] as usize,
            p.len() - 2 + d.output_len(),
            "{d:?}: outer DER length must cover the hash"
        );
        assert_eq!(p[2], 0x30, "{d:?}");
        assert_eq!(p[3] as usize, p.len() - 4 - 2, "{d:?}: inner DER length");
        assert_eq!(*p.last().unwrap() as usize, d.output_len(), "{d:?}");
        assert!(
            seen.insert(p),
            "{d:?} shares a DigestInfo prefix with another digest"
        );
    }
    assert_eq!(seen.len(), all.len());
}

fn assert_raises(result: CryptoResult<bool>, java_class: &str, what: &str) {
    match result {
        Err(e) => assert_eq!(e.java_class(), java_class, "{what}: wrong exception type"),
        Ok(v) => panic!("{what}: returned Ok({v}) instead of raising {java_class}"),
    }
}

#[test]
fn empty_modulus_raises_invalid_key() {
    let (_, e, sig) = fixture(b"m");
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&[], &e, DigestAlgorithm::Sha256, b"m", &sig),
        INVALID_KEY_EXCEPTION,
        "empty modulus",
    );
}

#[test]
fn zero_modulus_raises_invalid_key() {
    let (_, e, sig) = fixture(b"m");
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&[0u8; 128], &e, DigestAlgorithm::Sha256, b"m", &sig),
        INVALID_KEY_EXCEPTION,
        "all-zero modulus",
    );
}

#[test]
fn empty_or_zero_exponent_raises_invalid_key() {
    let (n, _, sig) = fixture(b"m");
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &[], DigestAlgorithm::Sha256, b"m", &sig),
        INVALID_KEY_EXCEPTION,
        "empty exponent",
    );
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &[0, 0, 0], DigestAlgorithm::Sha256, b"m", &sig),
        INVALID_KEY_EXCEPTION,
        "zero exponent",
    );
}

#[test]
fn even_exponent_raises_invalid_key() {
    let (n, _, sig) = fixture(b"m");
    // e = 4 is even; RFC 8017 requires an odd public exponent.
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &[4], DigestAlgorithm::Sha256, b"m", &sig),
        INVALID_KEY_EXCEPTION,
        "even exponent",
    );
}

/// The case that motivated this whole change: `RsaPublicKey::new` caps the
/// modulus at 4096 bits, so a **legitimate** larger key is rejected by the
/// backend. The old code reported that as `false` — "the signature did not
/// verify" — when in truth nothing was ever checked.
/// A 5008-bit odd big-endian magnitude — a plausible RSA modulus that is
/// past `RsaPublicKey::MAX_SIZE` (4096 bits). Built as raw bytes rather
/// than by `BigUint` arithmetic so the test does not depend on which
/// bignum backend `rsa` happens to re-export.
fn oversized_modulus_be() -> Vec<u8> {
    let mut m = vec![0u8; 626];
    m[0] = 0x80; // top bit set => exactly 5008 significant bits
    m[625] = 0x01; // odd (an even modulus would trip a different check)
    m
}

#[test]
fn oversized_but_legitimate_modulus_raises_invalid_key_not_false() {
    let (_, e, sig) = fixture(b"m");
    assert_raises(
        verify_rsa_pkcs1_v15_checked(
            &oversized_modulus_be(),
            &e,
            DigestAlgorithm::Sha256,
            b"m",
            &sig,
        ),
        INVALID_KEY_EXCEPTION,
        "modulus over MAX_SIZE",
    );
}

// ---- malformed signature encoding RAISES ----

#[test]
fn empty_signature_raises_signature_exception() {
    let (n, e, _) = fixture(b"m");
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &e, DigestAlgorithm::Sha256, b"m", &[]),
        SIGNATURE_EXCEPTION,
        "empty signature",
    );
}

#[test]
fn wrong_length_signature_raises_signature_exception() {
    let (n, e, sig) = fixture(b"m");
    let short = &sig[..sig.len() - 1];
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &e, DigestAlgorithm::Sha256, b"m", short),
        SIGNATURE_EXCEPTION,
        "truncated signature",
    );
    let mut long = sig.clone();
    long.push(0);
    assert_raises(
        verify_rsa_pkcs1_v15_checked(&n, &e, DigestAlgorithm::Sha256, b"m", &long),
        SIGNATURE_EXCEPTION,
        "over-long signature",
    );
}

// ---- the legacy bool wrapper is fail-closed ----

/// Every raising condition must collapse to `false`, never `true`, in the
/// compatibility wrapper. This is what makes the un-migrated signed-JAR
/// caller safe (if ambiguous) today.
#[test]
fn bool_wrapper_is_fail_closed_on_every_error_path() {
    let (n, e, sig) = fixture(b"m");
    let cases: Vec<(&str, Vec<u8>, Vec<u8>, Vec<u8>)> = vec![
        ("empty modulus", vec![], e.clone(), sig.clone()),
        ("zero exponent", n.clone(), vec![0], sig.clone()),
        ("even exponent", n.clone(), vec![4], sig.clone()),
        (
            "huge modulus",
            oversized_modulus_be(),
            e.clone(),
            sig.clone(),
        ),
        ("empty signature", n.clone(), e.clone(), vec![]),
        (
            "short signature",
            n.clone(),
            e.clone(),
            sig[..sig.len() - 1].to_vec(),
        ),
    ];
    for (what, nn, ee, ss) in cases {
        assert!(
            !verify_rsa_pkcs1_v15(&nn, &ee, DigestAlgorithm::Sha256, b"m", &ss),
            "{what}: bool wrapper must fail closed"
        );
    }
    // ...and it still returns true for the one case that deserves it.
    assert!(verify_rsa_pkcs1_v15(
        &n,
        &e,
        DigestAlgorithm::Sha256,
        b"m",
        &sig
    ));
}
