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
/// RSASSA-PSS is explicitly **not** here and must never be added: it is a
/// different padding scheme, and `Pkcs1v15Sign` would be the wrong verifier for
/// it.
///
/// # MD2 and MD5 are here, and the sentence they replace is worth keeping
///
/// This list said "explicitly not supported, and never to be added as a silent
/// alias: MD2, MD5 (JDK disables both for signatures via
/// `jdk.jar.disabledAlgorithms` / `jdk.certpath.disabledAlgorithms`)". The
/// operative words are SILENT ALIAS, and they still hold: nothing may map an
/// unrecognised name onto one of these, and no arm here may be a default.
///
/// What does not follow is refusing the names a caller spells out.
/// `jdk.jar.disabledAlgorithms` is a CERTPATH and JAR-verification policy, not
/// an engine capability — HotSpot 25's
/// `Signature.getInstance("MD5withRSA").sign()` returns bytes, and this VM
/// returning `SignatureException` instead is a portability difference, not a
/// security control (the same argument the `Cipher` seed makes for RC4 and
/// DES). `W7-63-jca-advertise-vs-serve.md` §3 #1 had already made this call for
/// the digest half: it implemented MD2 rather than de-advertising it, and said
/// why — "`SunRsaSign` and `SunMSCAPI` both advertise `MD2withRSA`". This is
/// the other half of that sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DigestAlgorithm {
    Md2,
    Md5,
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    /// FIPS 180-4 §5.3.6 — SHA-512 with a distinct initial hash value, NOT a
    /// truncation of SHA-512 and not SHA-224.
    Sha512_224,
    /// FIPS 180-4 §5.3.6, likewise distinct from SHA-256.
    Sha512_256,
    Sha3_224,
    Sha3_256,
    Sha3_384,
    Sha3_512,
}

impl DigestAlgorithm {
    /// The DER **content** bytes of this digest's object identifier — the value
    /// inside the `06` tag, without tag or length.
    pub fn oid_der(self) -> &'static [u8] {
        match self {
            // 1.2.840.113549.2.{2,5} — RSADSI digestAlgorithm arc.
            DigestAlgorithm::Md2 => &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x02],
            DigestAlgorithm::Md5 => &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05],
            // 1.3.14.3.2.26 — the OIW SHA-1 OID, the one outlier in this table.
            DigestAlgorithm::Sha1 => &[0x2b, 0x0e, 0x03, 0x02, 0x1a],
            // 2.16.840.1.101.3.4.2.N — the NIST hashAlgs arc, one N per digest.
            DigestAlgorithm::Sha256 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01],
            DigestAlgorithm::Sha384 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02],
            DigestAlgorithm::Sha512 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03],
            DigestAlgorithm::Sha224 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04],
            DigestAlgorithm::Sha512_224 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x05],
            DigestAlgorithm::Sha512_256 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x06],
            DigestAlgorithm::Sha3_224 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x07],
            DigestAlgorithm::Sha3_256 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x08],
            DigestAlgorithm::Sha3_384 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x09],
            DigestAlgorithm::Sha3_512 => &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x0a],
        }
    }

    /// This digest's output length in bytes.
    pub fn output_len(self) -> usize {
        match self {
            DigestAlgorithm::Md2 | DigestAlgorithm::Md5 => 16,
            DigestAlgorithm::Sha1 => 20,
            DigestAlgorithm::Sha224 | DigestAlgorithm::Sha512_224 | DigestAlgorithm::Sha3_224 => 28,
            DigestAlgorithm::Sha256 | DigestAlgorithm::Sha512_256 | DigestAlgorithm::Sha3_256 => 32,
            DigestAlgorithm::Sha384 | DigestAlgorithm::Sha3_384 => 48,
            DigestAlgorithm::Sha512 | DigestAlgorithm::Sha3_512 => 64,
        }
    }

    /// The PKCS#1 v1.5 `DigestInfo` DER prefix — everything before the hash
    /// bytes in RFC 8017 §9.2's `T`.
    ///
    /// **Generated, not transcribed.** RFC 8017 note 1 publishes these as
    /// fixed hex blobs and every implementation copies them, which is a
    /// transcription per digest with no way to check one but by eye. The
    /// structure is fixed:
    ///
    /// ```text
    /// SEQUENCE {                       30 L1
    ///   SEQUENCE {                     30 L2
    ///     OBJECT IDENTIFIER            06 len <oid>
    ///     NULL                         05 00
    ///   }
    ///   OCTET STRING                   04 hashLen
    /// }
    /// ```
    ///
    /// so `L2 = 2 + oidLen + 2` and `L1 = 2 + L2 + 2 + hashLen`, and the only
    /// per-digest inputs are the OID and the length —
    /// `generated_prefixes_match_the_rfc8017_constants` asserts that this
    /// reproduces the four published blobs byte for byte, which is what makes
    /// the other nine trustworthy.
    ///
    /// Every length here is one byte because the longest form is 51 bytes; a
    /// `debug_assert` pins that rather than leaving it implied.
    pub fn pkcs1v15_digest_info_prefix(self) -> Vec<u8> {
        let oid = self.oid_der();
        let hash_len = self.output_len();
        let l2 = 2 + oid.len() + 2;
        let l1 = 2 + l2 + 2 + hash_len;
        debug_assert!(l1 < 0x80, "DigestInfo needs a long-form DER length");
        let mut out = Vec::with_capacity(2 + l1);
        out.extend_from_slice(&[0x30, l1 as u8, 0x30, l2 as u8, 0x06, oid.len() as u8]);
        out.extend_from_slice(oid);
        out.extend_from_slice(&[0x05, 0x00, 0x04, hash_len as u8]);
        out
    }
}

/// The `rsa` crate's PKCS#1 v1.5 padding scheme for `digest`, built from
/// [`DigestAlgorithm::pkcs1v15_digest_info_prefix`].
///
/// `Pkcs1v15Sign::new::<D>()` is the usual construction and it needs
/// `D: Digest + AssociatedOid` — which pins the set of digests to those with a
/// Rust type in scope carrying an OID, and would have meant enabling `oid` on
/// `sha3` and adding `md2`/`md-5` here purely to name three constants. The
/// struct's `prefix` and `hash_len` are public, so the prefix this crate
/// already generates is enough, and the digest set stops depending on which
/// crates happen to be linked.
fn pkcs1v15_scheme(digest: DigestAlgorithm) -> Pkcs1v15Sign {
    Pkcs1v15Sign {
        hash_len: Some(digest.output_len()),
        prefix: digest.pkcs1v15_digest_info_prefix().into_boxed_slice(),
    }
}

/// [`verify_rsa_pkcs1_v15_checked`] for a caller that has ALREADY hashed the
/// message.
///
/// Exists because the digest set and the hashing code live in different crates:
/// `native-builtins` computes MD2, MD5 and the SHA-3 family (its `real_md2` /
/// `real_md5` and its `sha3` dependency), and it depends on this crate rather
/// than the reverse. Rather than a second implementation of those three digests
/// here, the caller hashes and this function owns the padding — which is the
/// half that must not be duplicated, since it is where a wrong DigestInfo
/// prefix would produce a signature nobody else accepts.
///
/// Every refusal in [`verify_rsa_pkcs1_v15_checked`] applies here identically
/// and for the same reasons; only the hashing moved.
pub fn verify_rsa_pkcs1_v15_prehashed(
    modulus_be: &[u8],
    exponent_be: &[u8],
    digest: DigestAlgorithm,
    hash: &[u8],
    signature: &[u8],
) -> CryptoResult<bool> {
    // A hash of the wrong length for the named digest is a caller bug, and
    // silently padding or truncating it would produce a verdict about a
    // different message. It cannot be a `false`: no security decision was made.
    if hash.len() != digest.output_len() {
        return Err(CryptoFailure::malformed_signature(format!(
            "{digest:?} produces {} bytes and was handed {}",
            digest.output_len(),
            hash.len()
        )));
    }
    let key = rsa_public_key_for_verify(modulus_be, exponent_be, signature)?;
    // PRESERVED NEGATIVE: see `verify_rsa_pkcs1_v15_checked`. A failure from
    // here on is "the padded digest did not match", which is an answer.
    Ok(key.verify(pkcs1v15_scheme(digest), hash, signature).is_ok())
}

/// The key/signature validation both PKCS#1 v1.5 verify entry points share,
/// lifted so there is one copy of each refusal and one place they are worded.
///
/// Every `Err` here means NO security decision was made — the distinction the
/// checked entry point exists for. See its table for which condition maps to
/// which JDK exception and why.
fn rsa_public_key_for_verify(
    modulus_be: &[u8],
    exponent_be: &[u8],
    signature: &[u8],
) -> CryptoResult<RsaPublicKey> {
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
    // Turning any of these into `false` reads at the call site as "the
    // signature did not verify" — a security decision never actually made.
    let key = RsaPublicKey::new(n, e)
        .map_err(|err| CryptoFailure::invalid_key(format!("RSA public key rejected: {err}")))?;
    // Signature length is a structural property of the encoding, not evidence
    // about the message. SunRsaSign raises SignatureException here.
    if signature.len() != key.size() {
        return Err(CryptoFailure::malformed_signature(format!(
            "Signature length not correct: got {} but was expecting {}",
            signature.len(),
            key.size()
        )));
    }
    Ok(key)
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
    // The four digests this crate can compute on its own. Everything else —
    // MD2, MD5 and the SHA-3 family — is hashed by the caller and arrives
    // through `verify_rsa_pkcs1_v15_prehashed`, because those implementations
    // live in `native-builtins`, which depends on this crate and not the other
    // way round. Adding `md2`/`md-5`/`sha3` here to close that would put a
    // second implementation of three digests in the tree.
    let hash: Vec<u8> = match digest {
        DigestAlgorithm::Sha1 => Sha1::digest(message).to_vec(),
        DigestAlgorithm::Sha256 => Sha256::digest(message).to_vec(),
        DigestAlgorithm::Sha384 => Sha384::digest(message).to_vec(),
        DigestAlgorithm::Sha512 => Sha512::digest(message).to_vec(),
        other => {
            return Err(crate::failure::CryptoFailure::no_such_algorithm(format!(
                "verify_rsa_pkcs1_v15_checked cannot hash {other:?}; the caller must                  hash it and use verify_rsa_pkcs1_v15_prehashed"
            )))
        }
    };
    // Every refusal — the key checks and the signature length — lives in
    // `verify_rsa_pkcs1_v15_prehashed` now, so there is one copy of each.
    verify_rsa_pkcs1_v15_prehashed(modulus_be, exponent_be, digest, &hash, signature)
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
}
