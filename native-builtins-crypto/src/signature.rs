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
    let key = RsaPublicKey::new(n, e).map_err(|err| {
        CryptoFailure::invalid_key(format!("RSA public key rejected: {err}"))
    })?;

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
/// The signed-JAR path (`classloading/src/jar_signer.rs:1516`) still calls
/// this and maps `false` onto `SigVerify::Bad`, so an unusable signer key is
/// currently reported as a bad signature rather than as an unverifiable one.
/// Both outcomes refuse the JAR, so the immediate behaviour is safe; the
/// migration to `verify_rsa_pkcs1_v15_checked` is tracked as a residual gap in
/// `docs/security/crypto-failure-contract.md`.
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
    use super::*;
    use rsa::pkcs1v15::SigningKey;
    use rsa::rand_core::OsRng;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;

    #[test]
    fn valid_and_tampered_sha256_signatures_are_distinguished() {
        let private = rsa::RsaPrivateKey::new(&mut OsRng, 1024).expect("test key");
        let public = private.to_public_key();
        let signer = SigningKey::<Sha256>::new(private);
        let sig = signer.sign(b"shared verifier").to_vec();
        assert!(verify_rsa_pkcs1_v15(
            &public.n().to_bytes_be(),
            &public.e().to_bytes_be(),
            DigestAlgorithm::Sha256,
            b"shared verifier",
            &sig,
        ));
        assert!(!verify_rsa_pkcs1_v15(
            &public.n().to_bytes_be(),
            &public.e().to_bytes_be(),
            DigestAlgorithm::Sha256,
            b"tampered",
            &sig,
        ));
    }
}
