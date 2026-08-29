// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared public-key verification used by both JCA and signed-JAR trust.
//!
//! Keeping the RFC 8017 parsing and verification here prevents the class
//! loader and `java.security.Signature` paths from drifting independently.

use crate::failure::{CryptoFailure, CryptoResult};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

/// The digest algorithms this verifier implements.
///
/// This is a **closed** set, and that is the point: there is no "unknown"
/// variant that could be silently treated as a default. An OID or JCA name the
/// caller cannot map onto one of these must be rejected by the caller with
/// `NoSuchAlgorithmException` (`crate::failure::NO_SUCH_ALGORITHM_EXCEPTION`)
/// *before* reaching this module — it must never be coerced to, say, SHA-1.
///
/// Explicitly **not** supported, and never to be added as a silent alias:
/// MD2, MD5 (JDK disables both for signatures via `jdk.jar.disabledAlgorithms`
/// / `jdk.certpath.disabledAlgorithms`), and RSASSA-PSS (a different padding
/// scheme entirely — `Pkcs1v15Sign` would be the wrong verifier for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

/// Verify an RSA PKCS#1 v1.5 signature over `message`, distinguishing a
/// **rejected key or malformed signature** (`Err`) from a **genuine
/// verification mismatch** (`Ok(false)`).
///
/// This is the authoritative entry point. The `bool`-returning
/// [`verify_rsa_pkcs1_v15`] below is retained only for callers that have not
/// migrated yet, and it necessarily collapses the two cases.
///
/// # Which condition maps to which JDK exception
///
/// | Condition | Result | Why |
/// |---|---|---|
/// | Empty/zero modulus or exponent | `Err(InvalidKeyException)` | Not a key. `RsaPublicKey::new` would reject it and the old code turned that into `false`. |
/// | `RsaPublicKey::new` rejects the components | `Err(InvalidKeyException)` | Covers an even exponent, `e < 2`, `e > 2^33-1`, and — importantly — a modulus over `RsaPublicKey::MAX_SIZE` (4096 bits). A **legitimate 8192-bit key** lands here; reporting that as "signature did not verify" is exactly the ambiguity this function exists to remove. |
/// | `signature.len() != key.size()` | `Err(SignatureException)` | SunRsaSign throws `SignatureException("Signature length not correct")`. A *genuine* mismatch always has the correct length, so this can never swallow a real negative. |
/// | Padding/digest mismatch | `Ok(false)` | **A real security decision.** The caller asked "does this signature match?" and the answer is no. Must NOT become an exception. |
/// | Signature matches | `Ok(true)` | — |
///
/// # TRUST BOUNDARY
///
/// A `true` here means one thing only: *these signature bytes are a valid
/// PKCS#1 v1.5 signature over this message under this public key*. It says
/// nothing about whether the key is trusted, whether its certificate chains to
/// an anchor, whether that chain is in date, or whether it has been revoked.
/// Callers establishing trust (signed JARs, code signing) must perform
/// certification-path validation separately; see
/// `docs/security/crypto-failure-contract.md` for the residual gap.
pub fn verify_rsa_pkcs1_v15_checked(
    modulus_be: &[u8],
    exponent_be: &[u8],
    digest: DigestAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> CryptoResult<bool> {
    let n = BigUint::from_bytes_be(modulus_be);
    let e = BigUint::from_bytes_be(exponent_be);

    // An absent or zero component is not a key at all. `BigUint::from_bytes_be`
    // maps both an empty slice and an all-zero slice to 0, so one check covers
    // both. Reject before `RsaPublicKey::new` so the message names the defect.
    if n == BigUint::from(0u8) {
        return Err(CryptoFailure::invalid_key(
            "RSA public key rejected: modulus is empty or zero",
        ));
    }
    if e == BigUint::from(0u8) {
        return Err(CryptoFailure::invalid_key(
            "RSA public key rejected: public exponent is empty or zero",
        ));
    }

    // `RsaPublicKey::new` enforces the remaining RFC 8017 / rsa-crate
    // constraints (odd exponent, 2 <= e <= 2^33-1, modulus <= MAX_SIZE bits).
    // The previous code turned every one of these into `false`, which reads at
    // the call site as "the signature did not verify" — a security decision we
    // never actually made.
    let key = RsaPublicKey::new(n, e)
        .map_err(|err| CryptoFailure::invalid_key(format!("RSA public key rejected: {err}")))?;

    // Signature length is a structural property of the encoding, not evidence
    // about the message. SunRsaSign raises SignatureException here; the rsa
    // crate would fold it into a generic verification error, which the old
    // `.is_ok()` then flattened into `false`.
    if signature.len() != key.size() {
        return Err(CryptoFailure::malformed_signature(format!(
            "Signature length not correct: got {} but was expecting {}",
            signature.len(),
            key.size()
        )));
    }

    let (scheme, hash): (Pkcs1v15Sign, Vec<u8>) = match digest {
        DigestAlgorithm::Sha1 => (Pkcs1v15Sign::new::<Sha1>(), Sha1::digest(message).to_vec()),
        DigestAlgorithm::Sha256 => (
            Pkcs1v15Sign::new::<Sha256>(),
            Sha256::digest(message).to_vec(),
        ),
        DigestAlgorithm::Sha384 => (
            Pkcs1v15Sign::new::<Sha384>(),
            Sha384::digest(message).to_vec(),
        ),
        DigestAlgorithm::Sha512 => (
            Pkcs1v15Sign::new::<Sha512>(),
            Sha512::digest(message).to_vec(),
        ),
    };
    // PRESERVED NEGATIVE: from here on, a failure means the padded digest did
    // not match — a legitimate `false`, not an exception. Turning this into an
    // error would break every caller that legitimately expects to be told "no".
    Ok(key.verify(scheme, &hash, signature).is_ok())
}

/// Verify an RSA PKCS#1 v1.5 signature over `message`.
///
/// **Deprecated in behaviour, retained in signature.** This wrapper cannot
/// distinguish "the key was rejected" from "the signature did not match" — a
/// `bool` has no room for the difference. It is fail-closed (any refusal
/// becomes `false`, never `true`), so it is safe, but it is *ambiguous*, and
/// ambiguity on a trust path is the defect. New code must call
/// [`verify_rsa_pkcs1_v15_checked`] and raise the named exception.
///
/// # TRUST BOUNDARY
///
/// **This function now has zero callers in the tree** — verified by grep over
/// every `.rs` file in the workspace; the only remaining occurrences of the
/// name are doc comments describing the migration away from it. Both former
/// call sites are gone:
///
/// * `classloading/src/jar_signer.rs` (`rsa_pkcs1v15_verify`) now calls
///   [`verify_rsa_pkcs1_v15_checked`] and maps `Err` to
///   `SigVerify::Unsupported` rather than `SigVerify::Bad`;
/// * `native-builtins/src/crypto_impl.rs` (`Rsa::try_verify_sha256`, behind
///   `rsa_verify`) does the same, returning `None` for "never checked".
///
/// It is therefore `#[deprecated]`: it is kept only so that an out-of-tree or
/// mid-flight consumer does not break, and so that anyone who reaches for it
/// gets told, at compile time, which function to use instead. Deleting it is
/// the follow-up once the branch settles.
#[deprecated(
    since = "0.3.0",
    note = "ambiguous on a trust path: a `false` cannot distinguish `the signature does not \
            match` from `the key was rejected and nothing was verified`. Call \
            verify_rsa_pkcs1_v15_checked and map Err onto a distinct `unverifiable` outcome. \
            This wrapper has no remaining in-tree callers."
)]
pub fn verify_rsa_pkcs1_v15(
    modulus_be: &[u8],
    exponent_be: &[u8],
    digest: DigestAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> bool {
    // Fail closed: `Err` must never surface as `true`. `matches!` is used
    // rather than `unwrap_or(false)` so this stays a deliberate, greppable
    // collapse rather than one of the `unwrap_or_default()` sites this audit
    // exists to remove.
    matches!(
        verify_rsa_pkcs1_v15_checked(modulus_be, exponent_be, digest, message, signature),
        Ok(true)
    )
}

#[cfg(test)]
mod tests {
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
    use rsa::pkcs1v15::SigningKey;
    use rsa::rand_core::OsRng;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;

    /// One 1024-bit key + a SHA-256 signature over `msg`, reused by the cases
    /// below (RSA keygen is the expensive part of this test module).
    fn fixture(msg: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let private = rsa::RsaPrivateKey::new(&mut OsRng, 1024).expect("test key");
        let public = private.to_public_key();
        let signer = SigningKey::<Sha256>::new(private);
        let sig = signer.sign(msg).to_vec();
        (public.n().to_bytes_be(), public.e().to_bytes_be(), sig)
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
        let private = rsa::RsaPrivateKey::new(&mut OsRng, 1024).expect("test key");
        let public = private.to_public_key();
        let (n, e) = (public.n().to_bytes_be(), public.e().to_bytes_be());
        let msg = b"multi-digest";

        let cases: Vec<(DigestAlgorithm, Vec<u8>)> = vec![
            (
                DigestAlgorithm::Sha1,
                SigningKey::<Sha1>::new(private.clone()).sign(msg).to_vec(),
            ),
            (
                DigestAlgorithm::Sha256,
                SigningKey::<Sha256>::new(private.clone())
                    .sign(msg)
                    .to_vec(),
            ),
            (
                DigestAlgorithm::Sha384,
                SigningKey::<Sha384>::new(private.clone())
                    .sign(msg)
                    .to_vec(),
            ),
            (
                DigestAlgorithm::Sha512,
                SigningKey::<Sha512>::new(private.clone())
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
}
