// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared public-key verification used by both JCA and signed-JAR trust.
//!
//! Keeping the RFC 8017 parsing and verification here prevents the class
//! loader and `java.security.Signature` paths from drifting independently.

use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

/// Verify an RSA PKCS#1 v1.5 signature over `message`.
///
/// Invalid keys, out-of-range signatures, malformed padding and digest
/// mismatches all return `false`; no caller gets a permissive parse fallback.
pub fn verify_rsa_pkcs1_v15(
    modulus_be: &[u8],
    exponent_be: &[u8],
    digest: DigestAlgorithm,
    message: &[u8],
    signature: &[u8],
) -> bool {
    let Ok(key) = RsaPublicKey::new(
        BigUint::from_bytes_be(modulus_be),
        BigUint::from_bytes_be(exponent_be),
    ) else {
        return false;
    };
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
    key.verify(scheme, &hash, signature).is_ok()
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
