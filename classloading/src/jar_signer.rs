// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JAR signer-block (PKCS#7 / CMS SignedData) verification.
//!
//! # Why this module exists
//!
//! Historically `class_path::extract_jar_signer_blocks` returned the raw
//! `*.RSA` / `*.DSA` / `*.EC` bytes verbatim and stored them as
//! `CodeSource.certificates`.  Those bytes are CMS SignedData containers,
//! **not** X.509 certificates — `Class.getCodeSource().getCertificates()`
//! was therefore exposing attacker-controlled opaque blobs and treating
//! them as trusted signer identities (HIGH severity auth-bypass).
//!
//! This module parses a signer block, walks the SignedData structure,
//! extracts the embedded leaf X.509 certificate, and verifies the
//! `messageDigest` authenticated attribute against the digest of the
//! matching `*.SF` signature file.  Only on success do we surface the
//! leaf certificate DER to the caller.
//!
//! # In scope for this commit (minimum viable fix)
//!
//! * PKCS#7 SignedData ASN.1 walking (`SignedData ::= SEQUENCE { version,
//!   digestAlgorithms, encapContentInfo, certificates [0] IMPLICIT,
//!   crls [1] IMPLICIT OPTIONAL, signerInfos SET OF SignerInfo }`)
//! * Leaf certificate DER extraction (first cert in the `certificates [0]`
//!   set — matches Sun jarsigner ordering, end-entity first).
//! * Signer principal extraction (the `IssuerAndSerialNumber` issuer DN).
//! * `messageDigest` authenticated attribute verification:
//!   `SHA-256(.SF)` or `SHA-1(.SF)` must match the digest in the signed
//!   attributes.  This is the integrity gate that catches `.SF` tampering
//!   without needing pub-key crypto.
//! * Reject (return `None`) for ANY parse error, truncation, OID mismatch,
//!   or digest mismatch — never panic.
//!
//! # What is real now (tasks #1 + #40 + jar-signer crypto)
//!
//! * **RSA public-key signature verification (PKCS#1 v1.5,
//!   SHA-1/256/384/512).**  The SignerInfo signature over the DER-encoded
//!   `SignedAttributes` is verified against the signer (leaf)
//!   certificate's public key, and every X.509 chain link's signature is
//!   verified against its issuer's public key.  The RSA math
//!   (`BigUint::modpow` + PKCS#1 v1.5 DigestInfo compare) is a
//!   self-contained, public-data-only implementation in this module — the
//!   `cratonvm-native-builtins` crypto primitives are unreachable from
//!   here (that crate depends on `classloading`, so importing it back
//!   would form a build cycle), and the workspace lock has no standalone
//!   `rsa` / `num-bigint` crate to reuse.
//! * **ECDSA signature verification (P-256 / P-384, SHA-256/384/512).**
//!   FEAT(jar-signer): the SignerInfo signature and each X.509 chain
//!   link signed with `ecdsa-with-SHA*` are verified with the audited
//!   RustCrypto `ecdsa` + `p256` / `p384` crates.  The curve is recovered
//!   from the SPKI `id-ecPublicKey` named-curve parameter; the wire
//!   signature is the DER `SEQUENCE { r, s }` form CMS / X.509 emit.
//! * **DSA (DSS) signature verification.**  FEAT(jar-signer): legacy
//!   `*.DSA` signer blocks and DSA-signed chain links are verified with
//!   the RustCrypto `dsa` crate.  The `(p, q, g)` domain parameters and
//!   the public value `y` are parsed from the SPKI; the signature is the
//!   DER `SEQUENCE { r, s }` form.  SHA-1 and SHA-256 DSA digests are
//!   supported (`id-dsa-with-sha1`, `id-dsa-with-sha256`).
//! * **RFC 5280 certification-path validation.**  FEAT(jar-signer):
//!   [`verify_chain`] now enforces, in addition to signature-link and
//!   validity-window checks: name-chaining (each cert's issuer DN must
//!   equal its parent's subject DN — already present), BasicConstraints
//!   (a present `cA=false` on a cert used to certify another is rejected;
//!   `pathLenConstraint` is honoured against the count of intervening
//!   non-self-issued CAs), KeyUsage (a CA cert with KeyUsage MUST assert
//!   `keyCertSign`; a leaf with KeyUsage MUST assert `digitalSignature`),
//!   and ExtendedKeyUsage (a leaf with an EKU MUST include
//!   `id-kp-codeSigning` or `anyExtendedKeyUsage`).  Unknown *critical*
//!   extensions on any cert cause rejection (fail-closed, RFC 5280
//!   §6.1.4 (f)).  Extensions are parsed with the audited `x509-cert`
//!   crate.  Posture for *absent* extensions is enforce-if-present (a
//!   legacy cert that omits BasicConstraints / KeyUsage is not rejected
//!   for the omission), matching stock HotSpot jarsigner.
//! * **Trust-store loading.**  [`TrustStore::load_default`] reads, in
//!   priority order: the `javax.net.ssl.trustStore` sys-prop (PEM, JKS,
//!   or PKCS#12 — auto-detected), a `CRATONVM_TRUST_PEM` PEM bundle, an
//!   `extend_from_anchors()` seam for the host VM's system root store,
//!   and the JDK `cacerts` (JKS, password `changeit`).  JKS is parsed by
//!   an in-module walker (integrity MAC verified first); PKCS#12 via the
//!   `p12` crate (PFX MAC verified first).
//! * **Chain validation.**  [`verify_chain`] walks leaf → intermediates →
//!   anchor by Subject↔Issuer DN match, cryptographically verifies each
//!   link's signature (RSA PKCS#1 v1.5, ECDSA P-256/P-384, or DSA — see
//!   the bullets above), checks each cert's `[notBefore, notAfter]`
//!   validity window against wall-clock time, and refuses any leaf with
//!   no path to a trust anchor.  Fail-closed throughout.
//!
//! # Residual gaps — precisely documented (fail-closed for all)
//!
//! * **Revocation checking (CRL / OCSP).**  RFC 5280 §6.3 revocation is
//!   NOT performed: a cert that chains and is in-validity is accepted
//!   even if its issuer has since revoked it.  This matches stock
//!   HotSpot jarsigner behaviour absent an explicit `-revCheck`, and
//!   revocation needs network I/O the classloader deliberately avoids.
//! * **NameConstraints / PolicyConstraints / policy mapping.**  RFC 5280
//!   §6.1.4 name-constraint and certificate-policy processing is not
//!   implemented.  Because an unprocessed `nameConstraints` is the only
//!   RFC-5280 extension whose *absence of enforcement* could broaden
//!   trust, and CAs mark it critical, such a chain is **rejected** by the
//!   unknown-critical-extension gate (fail-closed) rather than silently
//!   accepted.
//! * **DSA digests beyond SHA-1 / SHA-256.**  `id-dsa-with-sha224` and
//!   the SHA-384/512 DSA OIDs are recognised but rejected as
//!   unsupported (real DSA-signed JARs only use SHA-1 / SHA-256).
//! * **EC curves beyond P-256 / P-384.**  P-521, the Brainpool curves,
//!   and explicit-parameter EC keys surface as unsupported (no JAR
//!   signer in the wild uses them).
//! * **Multiple-signer SignerInfo dispatch.**  We only verify the first
//!   `SignerInfo`; multi-signer JARs (rare) collapse to "first signer
//!   verified".
//! * **Reading the `.SF` from a remote / nested location.**  Caller is
//!   responsible for feeding us the matching `.SF` bytes.
//!
//! # TRUST BOUNDARY — what a verified signer does and does not mean
//!
//! Audited in `docs/security/signed-jar-trust.md`; read that before
//! treating any result from this module as an authorisation decision.
//! The short form:
//!
//! * Every API here is labelled **trust** or **integrity** in its own
//!   doc comment.  The integrity ones ([`verify_sf_binds_manifest`],
//!   [`digest_matches`], [`parse_manifest_entry_digests`]) are digest
//!   comparisons with no key and no certificate anywhere in them.  They
//!   are useful and they are complete — but a `true` from them is not
//!   evidence about *who* produced the bytes, and they must not be read
//!   that way.
//! * The trust ones ([`verify_signer_block`], [`verify_chain`],
//!   [`verify_chain_path`]) do perform real certification-path
//!   validation: signature per link, anchor match, validity windows, and
//!   the RFC 5280 BasicConstraints / KeyUsage / EKU / unknown-critical
//!   rules.  What they do **not** do is revocation, name constraints, or
//!   policy processing — enumerated under "Residual gaps" below.
//! * Every verification result is three-valued, not boolean (see
//!   [`SigVerify`]).  "We checked and it failed" and "we never checked"
//!   are different answers and are kept different all the way to the
//!   caller; both refuse.
//! * Certification-path validation is *necessary* for trust and is not
//!   *sufficient* for it: a `Some(_)` from [`verify_signer_block`] covers
//!   the signer block and its `.SF`, and nothing else in the archive.
//!   Per-entry coverage is enforced in `class_path.rs`.
//!
//! # Hard limits
//!
//! * Signer-block input capped at 1 MiB (CMS DER blobs are typically <8 KiB).
//! * Recursion depth capped at 32 SEQUENCE / SET levels.
//! * Cert DER capped at 64 KiB per cert (CA certs run ~2 KiB).
//!
//! All limits return `None`, never panic.

#![allow(dead_code)] // SHA-1 paths only fire on legacy JARs not exercised
                     // by every test target in the workspace.

use crate::loader_flags;
use tracing::warn;

// ---------------------------------------------------------------------------
// Hard limits — every parse function checks these before allocating.
// ---------------------------------------------------------------------------

/// Maximum signer-block size we will parse (1 MiB).  Real PKCS#7 blocks
/// emitted by `jarsigner` are <10 KiB; anything larger is either
/// pathological or an attempted zip-bomb feeder.
const MAX_SIGNER_BLOCK: usize = 1024 * 1024;

/// Maximum nesting depth.  The deepest legitimate path is
/// SignedData → encapContentInfo → SEQUENCE → ... → leaf TLV; 32 is
/// comfortable headroom.
const MAX_DEPTH: usize = 32;

/// Maximum bytes for a single X.509 cert we will surface.
const MAX_CERT_DER: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// ASN.1 DER tag bytes (mirrors `native-builtins::jca::asn1` — duplicated
// here because `classloading` cannot depend on `native-builtins`).
// ---------------------------------------------------------------------------

const TAG_INTEGER: u8 = 0x02;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;
/// `[0] IMPLICIT` context-specific constructed tag — wraps the
/// `certificates` field of SignedData.
const TAG_CTX0: u8 = 0xA0;
/// `[1] IMPLICIT` — wraps the `crls` field of SignedData (we skip it).
const TAG_CTX1: u8 = 0xA1;

// ---------------------------------------------------------------------------
// OIDs we recognise in signer blocks.
// ---------------------------------------------------------------------------

/// `pkcs7-signedData` — content type of the outer ContentInfo of a JAR
/// signer block (`META-INF/*.RSA`, `*.DSA`, `*.EC`).
const OID_SIGNED_DATA: &str = "1.2.840.113549.1.7.2";

/// `pkcs7-data` — content type of the encapContentInfo (the JAR's `.SF`
/// content is *not* embedded; encapContentInfo is empty for detached
/// JAR signatures).
const OID_DATA: &str = "1.2.840.113549.1.7.1";

/// `pkcs9-messageDigest` — the authenticated attribute that carries the
/// `SHA-X(.SF)` value we verify.
const OID_MESSAGE_DIGEST: &str = "1.2.840.113549.1.9.4";

/// `pkcs9-contentType` — required authenticated attribute pinning the
/// content type to `pkcs7-data`.
const OID_CONTENT_TYPE: &str = "1.2.840.113549.1.9.3";

/// Digest algorithm OIDs.
const OID_SHA256: &str = "2.16.840.1.101.3.4.2.1";
const OID_SHA384: &str = "2.16.840.1.101.3.4.2.2";
const OID_SHA512: &str = "2.16.840.1.101.3.4.2.3";
const OID_SHA1: &str = "1.3.14.3.2.26";

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// A successfully verified signer.  Returned by [`verify_signer_block`]
/// on full self-consistency (signer-block parsed, leaf cert extracted,
/// `messageDigest` authenticated attribute matches the digest of the
/// caller-supplied `.SF` bytes).
#[derive(Debug, Clone)]
pub struct VerifiedSigner {
    /// DER-encoded X.509 certificate chain, end-entity first (matches
    /// the order Sun `jarsigner` emits).  `chain[0]` is the leaf.
    ///
    /// # TRUST BOUNDARY
    ///
    /// When [`verify_signer_block`] returns through a real (non-legacy)
    /// trust store this is **the validated certification path**, not the
    /// raw CMS `certificates` set: every element had its signature checked
    /// against its issuer, its validity window checked, and its RFC 5280
    /// extensions enforced, and the last element is a trust anchor.
    /// Certificates the signer block carried but that took no part in the
    /// path are dropped, because nothing about them was verified.
    ///
    /// Still **not** established for these certs: revocation status, name
    /// constraints, and certificate policies.
    pub chain: Vec<Vec<u8>>,
    /// Human-readable subject principal (best-effort UTF-8 of the
    /// `IssuerAndSerialNumber.issuer` Name, or `"<unparsed>"` if the
    /// RDN sequence isn't a single Common Name).  Surfaced to callers
    /// only; not used for cryptographic decisions.
    pub principal: String,
    /// Which digest algorithm bound the signer to the `.SF`.
    pub digest_alg: DigestAlg,
}

/// Digest algorithms we accept for the `.SF` binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlg {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Parse a JAR signer block (`META-INF/*.RSA|.DSA|.EC`) and verify it is
/// **both** self-consistent with the matching `*.SF` bytes **and**
/// chains to an anchor in `trust_store`.
///
/// `signer_block_der` is the raw signer-block file contents.  `sf_bytes`
/// is the corresponding signature-file (`*.SF`) contents — typically
/// found by stripping the extension and looking up `META-INF/{stem}.SF`
/// in the same JAR.  `trust_store` is the set of trusted root anchors;
/// see [`TrustStore::load_default`] for how it is populated.
///
/// Returns `Some(VerifiedSigner)` only when:
///   * the outer DER is a well-formed `SignedData` ContentInfo,
///   * at least one `SignerInfo` is present,
///   * the SignerInfo carries an `authenticatedAttributes` blob with both
///     the required `contentType=pkcs7-data` attribute and a
///     `messageDigest` attribute,
///   * the `messageDigest` attribute equals `H_alg(sf_bytes)` where
///     `alg` is the SignerInfo's declared `digestAlgorithm`,
///   * at least one X.509 certificate is embedded in the `certificates`
///     field,
///   * [`verify_chain`] finds a path from the leaf certificate through
///     the embedded intermediates to an anchor in `trust_store`.
///
/// Returns `None` on *any* failure (parse error, truncation, OID
/// mismatch, digest mismatch, missing cert, no trust path, ...).  Logs at
/// `tracing::warn` for diagnostics.  **Never panics.**
///
/// # Security limits
///
/// `signer_block_der.len()` is hard-capped at 1 MiB; larger inputs are
/// rejected immediately.  Cert DER blobs >64 KiB are dropped from the
/// returned chain (defense against single-cert zip-bombs).
///
/// # Trust-store mode for legacy unit tests
///
/// `TrustStore::permissive_legacy_tests` skips the chain step entirely —
/// used by the pre-task-#40 self-consistency tests below that embed
/// non-X.509-shaped marker certs.  It is `#[cfg(test)]`, so a production
/// build has no way to construct a permissive store; production code paths
/// supply a real trust store via [`TrustStore::load_default`].
///
/// # Cryptographic coverage
///
/// Pub-key signature verification over the authenticated-attributes blob
/// (when `enforce_pubkey` is set) and **cryptographic** verification of
/// each chain link's `signatureAlgorithm`-over-TBS-cert are both
/// implemented: RSA PKCS#1 v1.5 (SHA-1/256/384/512), ECDSA P-256/P-384
/// (SHA-1/256/384/512), and DSA (SHA-1/256) — see
/// [`verify_signature_with_spki`].  Only genuinely-unsupported
/// curve/key/digest combinations (P-521, Brainpool, explicit
/// ECParameters, an unusual DSA digest, ...) surface as
/// [`TrustError::NotImplemented`], which this function maps to `None`
/// (fail-closed: treated as failure, never success).
///
/// # TRUST BOUNDARY
///
/// **Proven** by a `Some(_)` return:
///
///   * the `.SF` bytes the caller supplied are the ones the signer
///     authenticated (`messageDigest` attribute over `H(sf_bytes)`);
///   * the SignerInfo signature over the DER `SignedAttributes` verifies
///     under the leaf certificate's public key, with a signature algorithm
///     this build really implements;
///   * the leaf chains to an anchor in `trust_store`, every link
///     cryptographically verified, every cert in date, and the RFC 5280
///     BasicConstraints / KeyUsage / ExtendedKeyUsage / unknown-critical
///     rules enforced for the role each cert played;
///   * `chain` contains **only** the certificates on that validated path.
///
/// **Not proven**, and therefore never to be inferred from a `Some(_)`:
///
///   * *revocation* — no CRL or OCSP is consulted, so a cert whose issuer
///     has revoked it still validates (RFC 5280 §6.3 is not implemented);
///   * *name constraints and certificate policies* — not processed.  A
///     chain whose CA marks `nameConstraints` **critical** (as CAs do) is
///     rejected by the unknown-critical-extension gate rather than being
///     accepted unprocessed, so the omission cannot silently broaden
///     trust; a non-critical one is ignored;
///   * *the rest of the JAR* — this function sees a signer block and a
///     `.SF`.  It says nothing about `MANIFEST.MF` or about any archive
///     entry.  Binding those is [`verify_sf_binds_manifest`] plus
///     [`parse_manifest_entry_digests`] / [`digest_matches`], and the
///     per-entry gate lives in
///     `class_path::ClassPath::certs_for_signed_class`.  A JAR entry that
///     is present in the archive but absent from the manifest is
///     **unsigned** no matter what this function returned;
///   * *additional signers* — only the first `SignerInfo` is examined.
///
/// A caller must not treat `Some(_)` alone as "this JAR is from a trusted
/// publisher".  See `docs/security/signed-jar-trust.md`.
pub fn verify_signer_block(
    signer_block_der: &[u8],
    sf_bytes: &[u8],
    trust_store: &TrustStore,
) -> Option<VerifiedSigner> {
    if signer_block_der.is_empty() || signer_block_der.len() > MAX_SIGNER_BLOCK {
        warn!(
            "jar signer: rejecting signer block of size {} (limit {})",
            signer_block_der.len(),
            MAX_SIGNER_BLOCK
        );
        return None;
    }
    // The public-key check over SignedAttributes is enforced in every
    // non-legacy trust-store mode (production).  The legacy self-
    // consistency fixtures embed marker-shaped "certs" and a fake RSA
    // signature, so they run with the pubkey gate off — exactly as they
    // already ran with the chain gate off.
    let enforce_pubkey = !trust_store.permissive_legacy;
    let mut vs = match parse_signed_data(signer_block_der, sf_bytes, enforce_pubkey) {
        Ok(vs) => vs,
        Err(e) => {
            warn!("jar signer: rejecting signer block: {}", e);
            return None;
        }
    };
    // Task #40: gate signer status on a chain-to-trust-anchor walk in
    // addition to the .SF self-consistency check above.  `TrustStore::
    // permissive_legacy_tests` short-circuits the walk for the older
    // self-consistency-only fixtures; every other trust store enforces.
    if !trust_store.permissive_legacy {
        // Borrow `vs.chain` only for the duration of the walk; the walk
        // hands back owned DER, so the reassignment below is unambiguous.
        let validated_path = {
            let leaf = match X509Cert::parse(&vs.chain[0]) {
                Ok(c) => c,
                Err(e) => {
                    warn!("jar signer: leaf cert is not parseable X.509: {}", e);
                    return None;
                }
            };
            let intermediates: Vec<X509Cert> = vs
                .chain
                .iter()
                .skip(1)
                .filter_map(|der| X509Cert::parse(der).ok())
                .collect();
            match verify_chain_path(&leaf, &intermediates, trust_store) {
                Ok(path) => path,
                Err(e) => {
                    warn!("jar signer: chain validation rejected leaf: {:?}", e);
                    return None;
                }
            }
        };
        // TRUST BOUNDARY: report only what was proven.  The CMS
        // `certificates` set is attacker-supplied and may carry extra
        // certificates that played no part in path construction; those were
        // never validated, so they must not travel out of here as part of
        // "the signer's certificates".  Narrowing `chain` to the validated
        // path is what makes `VerifiedSigner.chain` mean *every element of
        // this was checked* rather than *some element of this was checked*.
        vs.chain = validated_path;
    }
    Some(vs)
}

// ---------------------------------------------------------------------------
// PKCS#7 / CMS parsing
// ---------------------------------------------------------------------------

fn parse_signed_data(
    der: &[u8],
    sf_bytes: &[u8],
    enforce_pubkey: bool,
) -> Result<VerifiedSigner, &'static str> {
    // ContentInfo ::= SEQUENCE {
    //     contentType ContentType,
    //     content [0] EXPLICIT ANY DEFINED BY contentType
    // }
    let (content_info, _) = read_seq(der, 0)?;
    let mut cursor = Cursor::new(content_info);
    let oid = cursor.read_oid()?;
    if oid != OID_SIGNED_DATA {
        return Err("outer ContentInfo is not pkcs7-signedData");
    }
    // [0] EXPLICIT wrapper around SignedData SEQUENCE.
    let (ctx0, _) = cursor.read_tlv()?;
    if ctx0.tag != TAG_CTX0 {
        return Err("missing [0] EXPLICIT wrapper for SignedData");
    }
    let signed_data_seq = read_seq_strict(ctx0.content)?;
    let mut sd = Cursor::new(&signed_data_seq);

    // SignedData ::= SEQUENCE {
    //     version             CMSVersion,
    //     digestAlgorithms    SET OF DigestAlgorithmIdentifier,
    //     encapContentInfo    EncapsulatedContentInfo,
    //     certificates    [0] IMPLICIT CertificateSet OPTIONAL,
    //     crls            [1] IMPLICIT RevocationInfoChoices OPTIONAL,
    //     signerInfos         SET OF SignerInfo
    // }
    let _version = sd.read_integer()?;
    let _digest_algs = sd.read_set()?;
    let _encap = sd.read_seq_raw()?; // EncapsulatedContentInfo — detached, content is absent.

    // Optional certificates [0] IMPLICIT.
    let mut certs_der: Vec<Vec<u8>> = Vec::new();
    let peek_tag = sd.peek_tag();
    if peek_tag == Some(TAG_CTX0) {
        let (tlv, _) = sd.read_tlv()?;
        certs_der = split_certificate_set(tlv.content);
    }
    // Optional crls [1] IMPLICIT — skip.
    if sd.peek_tag() == Some(TAG_CTX1) {
        let _ = sd.read_tlv()?;
    }

    // signerInfos SET OF SignerInfo.
    let signer_infos = sd.read_set()?;
    let signer_infos = split_set_or_seq(&signer_infos, MAX_DEPTH)?;
    let first_si = signer_infos
        .into_iter()
        .next()
        .ok_or("SignedData has no SignerInfo")?;

    // SignerInfo ::= SEQUENCE {
    //     version            CMSVersion,
    //     sid                SignerIdentifier,
    //     digestAlgorithm    DigestAlgorithmIdentifier,
    //     signedAttrs   [0]  IMPLICIT SignedAttributes OPTIONAL,
    //     signatureAlgorithm SignatureAlgorithmIdentifier,
    //     signature          SignatureValue,
    //     unsignedAttrs [1]  IMPLICIT UnsignedAttributes OPTIONAL
    // }
    let signer_info = read_seq_strict(&first_si)?;
    let mut si = Cursor::new(&signer_info);
    let _si_version = si.read_integer()?;
    let sid_raw = si.read_seq_raw()?;
    let digest_alg_seq = si.read_seq_raw()?;
    let digest_alg_oid = first_oid_of_seq(&digest_alg_seq)?;
    let digest_alg =
        digest_alg_from_oid(&digest_alg_oid).ok_or("unsupported digest algorithm in SignerInfo")?;

    // Authenticated attributes [0] IMPLICIT SET OF Attribute.
    //
    // Per RFC 5652 §5.4, the signature is computed over the DER encoding
    // of the SignedAttributes with an explicit SET OF tag (0x31), *not*
    // the `[0] IMPLICIT` tag that appears on the wire.  We keep both the
    // content (for attribute walking) and the re-tagged SET (for the
    // pubkey signature check below).
    let auth_attrs = if si.peek_tag() == Some(TAG_CTX0) {
        let (tlv, _) = si.read_tlv()?;
        Some(tlv.content.to_vec())
    } else {
        None
    };
    let auth_attrs = auth_attrs.ok_or(
        "SignerInfo is missing authenticatedAttributes — refusing to skip integrity check",
    )?;
    // DER re-encoding of SignedAttributes as an explicit SET OF Attribute.
    let signed_attrs_der = encode_tlv(TAG_SET, &auth_attrs);

    // SignerInfo continues: signatureAlgorithm, signature.
    let sig_alg_seq = si.read_seq_raw()?;
    let signer_sig_alg_oid = first_oid_of_seq(&sig_alg_seq)?;
    let (sig_tlv, _) = si.read_tlv()?;
    if sig_tlv.tag != TAG_OCTET_STRING {
        return Err("SignerInfo signature is not OCTET STRING");
    }
    let signer_signature = sig_tlv.content.to_vec();

    // Walk attributes; require contentType = pkcs7-data AND messageDigest matching SHA(.SF).
    let attrs = split_set_or_seq(&auth_attrs, MAX_DEPTH)?;
    let mut saw_content_type = false;
    let mut got_message_digest: Option<Vec<u8>> = None;
    for attr_tlv in attrs {
        let attr_seq = read_seq_strict(&attr_tlv)?;
        let mut a = Cursor::new(&attr_seq);
        let attr_oid = a.read_oid()?;
        let attr_values = a.read_set()?;
        match attr_oid.as_str() {
            OID_CONTENT_TYPE => {
                // SET OF OID containing pkcs7-data.
                let mut v = Cursor::new(&attr_values);
                let inner = v.read_oid()?;
                if inner != OID_DATA {
                    return Err("contentType attribute is not pkcs7-data");
                }
                saw_content_type = true;
            }
            OID_MESSAGE_DIGEST => {
                // SET OF OCTET STRING.
                let mut v = Cursor::new(&attr_values);
                let (octet_tlv, _) = v.read_tlv()?;
                if octet_tlv.tag != TAG_OCTET_STRING {
                    return Err("messageDigest attribute value is not OCTET STRING");
                }
                got_message_digest = Some(octet_tlv.content.to_vec());
            }
            _ => {
                // Other attributes (signingTime, etc.) are ignored.
            }
        }
    }
    if !saw_content_type {
        return Err("authenticatedAttributes missing required contentType");
    }
    let stored_digest =
        got_message_digest.ok_or("authenticatedAttributes missing messageDigest")?;

    // Re-digest the caller-supplied `.SF` bytes and constant-time-compare.
    let recomputed = raw_digest(digest_alg, sf_bytes);
    if recomputed.is_empty() || !ct_eq(&stored_digest, &recomputed) {
        return Err(".SF digest in messageDigest does not match SHA(SF) — tampered .SF");
    }

    // Integrity of the SignerInfo ↔ .SF binding is confirmed.  Now do the
    // real public-key check: the signer's certificate (leaf, the first
    // embedded cert per Sun jarsigner ordering) must sign the DER-encoded
    // SignedAttributes blob.  In `enforce_pubkey` mode (production) a
    // failure or unsupported algorithm is fatal — fail-closed.  Legacy
    // self-consistency fixtures pass `enforce_pubkey=false`.
    if certs_der.is_empty() {
        return Err("SignedData has no embedded certificates");
    }
    if enforce_pubkey {
        let leaf = X509Cert::parse(&certs_der[0])
            .map_err(|_| "signer leaf certificate is not parseable X.509")?;
        // Real jarsigner SignerInfos often carry a *bare* key-algorithm OID
        // in `signatureAlgorithm` (`rsaEncryption` 1.2.840.113549.1.1.1 /
        // `id-ecPublicKey` 1.2.840.10045.2.1) and leave the digest implied
        // by the SignerInfo `digestAlgorithm`.  Normalise those to the
        // combined `<digest>With<key>` OID our verifier understands.
        let effective_sig_oid =
            normalize_signer_sig_alg(&signer_sig_alg_oid, digest_alg).unwrap_or(signer_sig_alg_oid);
        match verify_signature_with_spki(
            leaf.spki_der,
            &effective_sig_oid,
            &signed_attrs_der,
            &signer_signature,
        ) {
            SigVerify::Ok => {}
            SigVerify::Bad => {
                // A verifier ran and rejected the bytes — a genuine negative.
                return Err("SignerInfo signature does not verify against signer public key");
            }
            SigVerify::Unsupported => {
                // TRUST BOUNDARY: nothing was verified here, so this is NOT
                // "the signature is bad" — it is "we have no evidence either
                // way".  RSA, ECDSA P-256/P-384 and DSA SHA-1/256 are really
                // verified; this arm fires for a curve/digest/key combination
                // outside that set (P-521, Brainpool, explicit ECParameters,
                // an unusual DSA digest), an unparseable SPKI, a key the
                // backend refused (e.g. an RSA modulus over 4096 bits), or a
                // malformed signature encoding.  Absence of evidence is not
                // evidence of trust: refuse, with a message that does not
                // claim a verdict we did not reach.
                return Err("SignerInfo signature could not be verified \
                     (unsupported or unusable algorithm/key) — refusing");
            }
        }
    }
    let principal = principal_from_sid(sid_raw).unwrap_or_else(|| "<unparsed>".to_string());
    Ok(VerifiedSigner {
        chain: certs_der,
        principal,
        digest_alg,
    })
}

/// Encode a single DER TLV: `tag || length || content`.
fn encode_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let len = content.len();
    let mut out = Vec::with_capacity(len + 4);
    out.push(tag);
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.push(0x81);
        out.push(len as u8);
    } else if len < 0x10000 {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    } else {
        out.push(0x83);
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push(len as u8);
    }
    out.extend_from_slice(content);
    out
}

/// Constant-time byte-slice equality.  Equal-length only.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn digest_alg_from_oid(oid: &str) -> Option<DigestAlg> {
    match oid {
        OID_SHA256 => Some(DigestAlg::Sha256),
        OID_SHA384 => Some(DigestAlg::Sha384),
        OID_SHA512 => Some(DigestAlg::Sha512),
        OID_SHA1 => Some(DigestAlg::Sha1),
        _ => None,
    }
}

// NB: digest computation now lives in the free function `raw_digest`,
// which covers all four SHA variants (SHA-1/256/384/512) via the local
// implementations + the `sha2` crate.  The old `DigestAlg::digest` method
// that returned an empty vec for SHA-384/512 has been removed.

/// Best-effort principal extraction from the `SignerIdentifier`.
///
/// SignerIdentifier ::= CHOICE {
///     issuerAndSerialNumber IssuerAndSerialNumber,
///     subjectKeyIdentifier [0] SubjectKeyIdentifier
/// }
///
/// We only handle the IssuerAndSerialNumber case and yield a flattened
/// `"<RDN>=<value>, <RDN>=<value>"` rendering of the issuer Name.  If
/// the structure does not match, returns `None` — the caller substitutes
/// `<unparsed>`.
fn principal_from_sid(sid_seq: Vec<u8>) -> Option<String> {
    // IssuerAndSerialNumber ::= SEQUENCE { issuer Name, serialNumber INTEGER }
    let mut c = Cursor::new(&sid_seq);
    let issuer_raw = c.read_seq_raw().ok()?;
    let _serial = c.read_integer().ok()?;
    // Name ::= SEQUENCE OF RDN; render each AttributeTypeAndValue as
    // "<oid-or-name>=<value>".
    let rdns = split_set_or_seq(&issuer_raw, MAX_DEPTH).ok()?;
    let mut parts = Vec::new();
    for rdn in rdns {
        // RDN ::= SET OF AttributeTypeAndValue.  Strip the outer SET
        // header and walk each ATV.
        let (rdn_tlv, total) = read_tlv(&rdn).ok()?;
        if rdn_tlv.tag != TAG_SET || total != rdn.len() {
            return None;
        }
        let atv_blobs = split_set_or_seq(rdn_tlv.content, MAX_DEPTH).ok()?;
        if let Some(first) = atv_blobs.first() {
            let atv = read_seq_strict(first).ok()?;
            let mut a = Cursor::new(&atv);
            let oid = a.read_oid().ok()?;
            let (val_tlv, _) = a.read_tlv().ok()?;
            let value = match std::str::from_utf8(val_tlv.content) {
                Ok(s) => s.to_string(),
                Err(_) => val_tlv.content.iter().map(|&b| b as char).collect(),
            };
            parts.push(format!("{}={}", short_oid(&oid), value));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Short human-readable name for a common X.500 attribute OID.
fn short_oid(oid: &str) -> &str {
    match oid {
        "2.5.4.3" => "CN",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "1.2.840.113549.1.9.1" => "EMAIL",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Minimal DER TLV reader
// ---------------------------------------------------------------------------

/// One DER-decoded TLV element.  `total` covers tag+length+content.
#[derive(Debug)]
struct Tlv<'a> {
    tag: u8,
    /// Content bytes (excludes the tag and length header).
    content: &'a [u8],
    /// Total length of `tag || len || content` — useful for advancing
    /// parent cursors past this element.
    total: usize,
}

/// Lightweight cursor over a DER blob.
struct Cursor<'a> {
    /// Remaining bytes (advanced by `read_*` calls).  Stored as `&[u8]`
    /// so all returned slices share its lifetime.
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new<T: AsRef<[u8]> + ?Sized>(s: &'a T) -> Self {
        Cursor { rest: s.as_ref() }
    }

    fn peek_tag(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    fn read_tlv(&mut self) -> Result<(Tlv<'a>, ()), &'static str> {
        let (tlv, advance) = read_tlv(self.rest)?;
        self.rest = &self.rest[advance..];
        Ok((tlv, ()))
    }

    /// Read a SEQUENCE TLV and return its content slice.
    fn read_seq_raw(&mut self) -> Result<Vec<u8>, &'static str> {
        let (tlv, _) = self.read_tlv()?;
        if tlv.tag != TAG_SEQUENCE {
            return Err("expected SEQUENCE");
        }
        Ok(tlv.content.to_vec())
    }

    /// Read a SET TLV and return its content slice.
    fn read_set(&mut self) -> Result<Vec<u8>, &'static str> {
        let (tlv, _) = self.read_tlv()?;
        if tlv.tag != TAG_SET {
            return Err("expected SET");
        }
        Ok(tlv.content.to_vec())
    }

    /// Read an INTEGER and return its content bytes (unsigned big-endian).
    fn read_integer(&mut self) -> Result<Vec<u8>, &'static str> {
        let (tlv, _) = self.read_tlv()?;
        if tlv.tag != TAG_INTEGER {
            return Err("expected INTEGER");
        }
        Ok(tlv.content.to_vec())
    }

    /// Read an OID and return its dotted-decimal form.
    fn read_oid(&mut self) -> Result<String, &'static str> {
        let (tlv, _) = self.read_tlv()?;
        if tlv.tag != TAG_OID {
            return Err("expected OID");
        }
        decode_oid(tlv.content)
    }
}

/// Decode a single DER TLV from `buf`.  Returns the TLV view and how
/// many bytes were consumed (== `tlv.total`).
fn read_tlv(buf: &[u8]) -> Result<(Tlv<'_>, usize), &'static str> {
    if buf.is_empty() {
        return Err("truncated TLV (no tag)");
    }
    let tag = buf[0];
    if buf.len() < 2 {
        return Err("truncated TLV (no length)");
    }
    let l0 = buf[1];
    let (content_len, hdr_extra) = if l0 < 0x80 {
        (l0 as usize, 1usize)
    } else if l0 == 0x80 {
        // Indefinite length — forbidden in DER.
        return Err("indefinite length not allowed in DER");
    } else {
        let n = (l0 & 0x7F) as usize;
        if n == 0 || n > 4 {
            return Err("invalid long-form length");
        }
        if buf.len() < 2 + n {
            return Err("truncated TLV (short length bytes)");
        }
        let mut len = 0usize;
        for &b in &buf[2..2 + n] {
            len = (len << 8) | (b as usize);
        }
        (len, 1 + n)
    };
    let hdr = 1 + hdr_extra;
    let total = hdr.checked_add(content_len).ok_or("TLV length overflow")?;
    if total > MAX_SIGNER_BLOCK {
        return Err("TLV exceeds MAX_SIGNER_BLOCK");
    }
    if buf.len() < total {
        return Err("truncated TLV (short content)");
    }
    Ok((
        Tlv {
            tag,
            content: &buf[hdr..total],
            total,
        },
        total,
    ))
}

/// Parse `buf` as a single top-level SEQUENCE and return `(content, _)`.
fn read_seq(buf: &[u8], depth: usize) -> Result<(&[u8], usize), &'static str> {
    if depth > MAX_DEPTH {
        return Err("nested too deep");
    }
    let (tlv, total) = read_tlv(buf)?;
    if tlv.tag != TAG_SEQUENCE {
        return Err("expected top-level SEQUENCE");
    }
    Ok((tlv.content, total))
}

/// Like `read_seq` but takes ownership of a single complete TLV byte
/// vector that's already known to be a SEQUENCE; returns the content
/// slice.  Used after `read_tlv` has already validated the framing.
fn read_seq_strict(buf: &[u8]) -> Result<Vec<u8>, &'static str> {
    let (tlv, total) = read_tlv(buf)?;
    if tlv.tag != TAG_SEQUENCE {
        return Err("expected SEQUENCE");
    }
    if total != buf.len() {
        return Err("trailing bytes after SEQUENCE");
    }
    Ok(tlv.content.to_vec())
}

/// Split a SET / SEQUENCE OF blob into its element TLVs (each returned
/// as the complete TLV bytes — caller re-decodes per element).
fn split_set_or_seq(buf: &[u8], depth: usize) -> Result<Vec<Vec<u8>>, &'static str> {
    if depth == 0 {
        return Err("nested too deep");
    }
    let mut out = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        let (tlv, total) = read_tlv(rest)?;
        out.push(rest[..total].to_vec());
        // `tlv.total == total`; drop it after use to silence the unused warning.
        let _ = tlv;
        rest = &rest[total..];
    }
    Ok(out)
}

/// Extract every X.509 cert from a CMS `certificates [0] IMPLICIT
/// CertificateSet` field.  CertificateSet ::= SET OF CertificateChoices;
/// we keep only the plain X.509 cases (tag = SEQUENCE) and skip the
/// `[n]` other-cert variants (`AttributeCertificate`,
/// `OtherCertificateFormat`).
///
/// Cert DER blobs are capped at `MAX_CERT_DER` — oversized certs are
/// silently dropped.
fn split_certificate_set(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = buf;
    while !rest.is_empty() {
        match read_tlv(rest) {
            Ok((tlv, total)) => {
                if tlv.tag == TAG_SEQUENCE && total <= MAX_CERT_DER {
                    out.push(rest[..total].to_vec());
                }
                rest = &rest[total..];
            }
            Err(_) => break,
        }
    }
    out
}

/// Pull the first OID out of an AlgorithmIdentifier-shaped SEQUENCE
/// (`SEQUENCE { algorithm OID, parameters ANY OPTIONAL }`).
fn first_oid_of_seq(seq_content: &[u8]) -> Result<String, &'static str> {
    let mut c = Cursor::new(seq_content);
    c.read_oid()
}

/// Decode an OID content blob into dotted-decimal form (e.g.
/// `1.2.840.113549.1.7.2`).
fn decode_oid(content: &[u8]) -> Result<String, &'static str> {
    if content.is_empty() {
        return Err("empty OID");
    }
    let first = content[0] as u64;
    let arc1 = first / 40;
    let arc2 = first % 40;
    use std::fmt::Write;
    let mut out = String::with_capacity(16);
    let _ = write!(out, "{}.{}", arc1, arc2);
    let mut i = 1;
    while i < content.len() {
        let mut v: u64 = 0;
        let mut bytes_in_arc = 0;
        loop {
            if i >= content.len() {
                return Err("truncated OID arc");
            }
            let b = content[i];
            i += 1;
            // Cap a single arc at 10 base-128 bytes (70 bits) to avoid
            // u64 overflow from a malicious OID.
            bytes_in_arc += 1;
            if bytes_in_arc > 10 {
                return Err("OID arc overflow");
            }
            v = (v << 7) | ((b & 0x7F) as u64);
            if (b & 0x80) == 0 {
                break;
            }
        }
        let _ = write!(out, ".{}", v);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SHA-1 — needed for legacy `.SF` digests; kept private to this module.
// FIPS 180-4 §6.1.
// ---------------------------------------------------------------------------

mod sha1 {
    pub fn digest(data: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
        let mut msg = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in msg.chunks_exact(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
            for (i, &word) in w.iter().enumerate().take(80) {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A827999),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                    _ => (b ^ c ^ d, 0xCA62C1D6),
                };
                let t = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(word);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = t;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }
        let mut out = [0u8; 20];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

// ---------------------------------------------------------------------------
// SHA-256 — duplicated here because the existing `class::sha256_hex` is
// private and returns a hex string; we need the 32-byte raw digest.
// ---------------------------------------------------------------------------

mod sha256 {
    pub fn digest(data: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut msg = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in msg.chunks_exact(64) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
                (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }
        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

// ---------------------------------------------------------------------------
// SHA-384 / SHA-512 — delegated to the `sha2` crate (already in the
// workspace lock via `native-builtins`).  Used by modern jarsigner `.SF`
// bindings and RSA PKCS#1 v1.5 DigestInfo verification.
// ---------------------------------------------------------------------------

mod sha2ext {
    use sha2::{Digest, Sha384, Sha512};

    pub fn sha384(data: &[u8]) -> [u8; 48] {
        let out = Sha384::digest(data);
        let mut r = [0u8; 48];
        r.copy_from_slice(&out);
        r
    }

    pub fn sha512(data: &[u8]) -> [u8; 64] {
        let out = Sha512::digest(data);
        let mut r = [0u8; 64];
        r.copy_from_slice(&out);
        r
    }
}

// ---------------------------------------------------------------------------
// Task #1 — public-key signature verification (RSA PKCS#1 v1.5).
//
// The RSA/ECDSA primitives in `cratonvm-native-builtins::crypto_impl` are
// unreachable from `classloading`: `native-builtins` already depends on
// `classloading` (see its `Cargo.toml`), so importing it back here would
// form a crate cycle the workspace cannot build.  The workspace lock also
// does NOT contain a stand-alone `rsa` / `num-bigint` / `p256` / `ecdsa`
// crate (native-builtins hand-rolls those on an in-tree `BigUint`).
//
// RSA *verification* is pure public-key arithmetic — modular exponentiation
// of public data with a public exponent and modulus, plus a PKCS#1 v1.5
// DigestInfo structural compare.  There is no secret material and therefore
// no timing-side-channel concern, so a self-contained `BigUint::modpow`
// here is the correct and safe reachable implementation (it mirrors the
// math `native-builtins::crypto_impl::Rsa::verify_sha256` performs).
//
// FEAT(jar-signer): ECDSA (P-256 / P-384) and DSA verification are now
// real, delegated to the audited RustCrypto `ecdsa` + `p256` / `p384` and
// `dsa` crates respectively.  Only RSA stays on the in-module `BigUint`
// (it has no external `rsa` crate in the lock and the math is pure
// public-data modexp — no secret-dependent timing concern).
// ---------------------------------------------------------------------------

/// Minimal unsigned big-integer over little-endian u32 limbs.  Only the
/// operations RSA verification needs are implemented: big-endian
/// (de)serialisation, multiply, modulo, and modular exponentiation.
#[derive(Clone, Debug)]
struct BigUint {
    /// Little-endian limbs; no trailing-zero limbs after `normalize`.
    limbs: Vec<u32>,
}

impl BigUint {
    fn zero() -> Self {
        BigUint { limbs: Vec::new() }
    }
    fn one() -> Self {
        BigUint { limbs: vec![1] }
    }
    fn is_zero(&self) -> bool {
        self.limbs.iter().all(|&l| l == 0)
    }
    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    fn from_bytes_be(bytes: &[u8]) -> Self {
        let mut limbs = Vec::with_capacity(bytes.len() / 4 + 1);
        // Walk from least-significant byte, packing 4 bytes per limb.
        let mut i = bytes.len();
        while i > 0 {
            let lo = i.saturating_sub(4);
            let mut limb = 0u32;
            for &b in &bytes[lo..i] {
                limb = (limb << 8) | b as u32;
            }
            limbs.push(limb);
            i = lo;
        }
        let mut r = BigUint { limbs };
        r.normalize();
        r
    }

    /// Big-endian byte serialisation, left-zero-padded to `k` bytes.
    fn to_bytes_be_padded(&self, k: usize) -> Vec<u8> {
        let mut out = vec![0u8; k];
        // Emit limbs from least significant; write into the tail of `out`.
        let mut pos = k;
        for &limb in &self.limbs {
            for s in 0..4 {
                if pos == 0 {
                    break;
                }
                pos -= 1;
                out[pos] = (limb >> (s * 8)) as u8;
            }
        }
        out
    }

    fn bit_length(&self) -> usize {
        match self.limbs.last() {
            None => 0,
            Some(&top) => (self.limbs.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
        }
    }

    fn bit(&self, idx: usize) -> bool {
        let limb = idx / 32;
        let off = idx % 32;
        self.limbs.get(limb).map_or(false, |&l| (l >> off) & 1 == 1)
    }

    fn cmp(&self, other: &BigUint) -> std::cmp::Ordering {
        let a = self.effective_len();
        let b = other.effective_len();
        if a != b {
            return a.cmp(&b);
        }
        for i in (0..a).rev() {
            let x = self.limbs[i];
            let y = other.limbs[i];
            if x != y {
                return x.cmp(&y);
            }
        }
        std::cmp::Ordering::Equal
    }
    fn effective_len(&self) -> usize {
        let mut n = self.limbs.len();
        while n > 0 && self.limbs[n - 1] == 0 {
            n -= 1;
        }
        n
    }

    fn sub(&self, other: &BigUint) -> BigUint {
        // Assumes self >= other.
        let mut out = Vec::with_capacity(self.limbs.len());
        let mut borrow: i64 = 0;
        for i in 0..self.limbs.len() {
            let a = self.limbs[i] as i64;
            let b = *other.limbs.get(i).unwrap_or(&0) as i64;
            let mut cur = a - b - borrow;
            if cur < 0 {
                cur += 1i64 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(cur as u32);
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    fn mul(&self, other: &BigUint) -> BigUint {
        if self.is_zero() || other.is_zero() {
            return BigUint::zero();
        }
        let mut out = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry: u64 = 0;
            for (j, &b) in other.limbs.iter().enumerate() {
                let cur = out[i + j] as u64 + (a as u64) * (b as u64) + carry;
                out[i + j] = cur as u32;
                carry = cur >> 32;
            }
            out[i + other.limbs.len()] += carry as u32;
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    fn shl_one_bit(&self) -> BigUint {
        let mut out = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry = 0u32;
        for &l in &self.limbs {
            out.push((l << 1) | carry);
            carry = l >> 31;
        }
        if carry != 0 {
            out.push(carry);
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    fn set_bit0(&mut self) {
        if self.limbs.is_empty() {
            self.limbs.push(1);
        } else {
            self.limbs[0] |= 1;
        }
    }

    /// `self mod m` via bitwise long division.  `m` must be non-zero.
    fn modulo(&self, m: &BigUint) -> BigUint {
        if m.is_zero() {
            return BigUint::zero();
        }
        if self.cmp(m) == std::cmp::Ordering::Less {
            return self.clone();
        }
        let mut rem = BigUint::zero();
        for i in (0..self.bit_length()).rev() {
            rem = rem.shl_one_bit();
            if self.bit(i) {
                rem.set_bit0();
            }
            if rem.cmp(m) != std::cmp::Ordering::Less {
                rem = rem.sub(m);
            }
        }
        rem.normalize();
        rem
    }

    /// `self^exp mod m` (square-and-multiply).  Public-data only.
    fn modpow(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        if m.cmp(&BigUint::one()) != std::cmp::Ordering::Greater {
            return BigUint::zero();
        }
        let mut result = BigUint::one();
        let base = self.modulo(m);
        let bits = exp.bit_length();
        let mut acc = base;
        for i in 0..bits {
            if exp.bit(i) {
                result = result.mul(&acc).modulo(m);
            }
            // Square for the next bit (skip after the final useful bit).
            if i + 1 < bits {
                acc = acc.mul(&acc).modulo(m);
            }
        }
        result
    }
}

/// A parsed RSA public key (modulus + exponent).
struct RsaPublicKey {
    n: BigUint,
    e: BigUint,
    /// Modulus size in bytes (`k` in PKCS#1) — the expected signature length.
    k: usize,
}

/// The public-key flavour recovered from a `SubjectPublicKeyInfo`.
enum PublicKey {
    Rsa(RsaPublicKey),
    /// FEAT(jar-signer): NIST P-256 (secp256r1) EC public key.  Carries the
    /// full DER `SubjectPublicKeyInfo` so the RustCrypto `p256` decoder can
    /// re-validate the point on the curve.
    EcP256(Vec<u8>),
    /// FEAT(jar-signer): NIST P-384 (secp384r1) EC public key (full SPKI DER).
    EcP384(Vec<u8>),
    /// FEAT(jar-signer): DSA (DSS) public key (full SPKI DER); the `dsa`
    /// crate recovers `(p, q, g, y)` from it.
    Dsa(Vec<u8>),
    /// An EC key on a curve we do not verify (P-521, Brainpool, ...), or
    /// any other key type.  Fail-closed.
    Other,
}

/// Outcome of a public-key signature verification attempt.
///
/// # The three-valued contract
///
/// This enum exists to keep **"we checked and the answer is no"** distinct
/// from **"we never checked"**.  Collapsing the two into a `bool` is the
/// defect this type prevents: at the call site a `false` that means *the
/// key was unusable* reads identically to a `false` that means *this is a
/// forgery*, and the second is a security decision while the first is the
/// absence of one.  See `docs/security/signed-jar-trust.md` §2 and
/// `docs/security/crypto-failure-contract.md` §1.
///
/// | Variant | Meaning | Verified? |
/// |---|---|---|
/// | [`SigVerify::Ok`] | Valid signature under this key. | Yes — positive. |
/// | [`SigVerify::Bad`] | The bytes do not match. **A real decision.** | Yes — negative. |
/// | [`SigVerify::Unsupported`] | Nothing was verified. | **No.** |
///
/// All three are handled explicitly at every call site; `Bad` and
/// `Unsupported` both refuse, and there is deliberately no `_ =>` arm that
/// could let a fourth state default into acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigVerify {
    /// Signature is cryptographically valid.
    Ok,
    /// Signature did not verify against the key.  **A genuine negative:**
    /// the full verification ran and the padded digest did not match.
    /// Never used for "the input was unusable" — that is
    /// [`SigVerify::Unsupported`].
    Bad,
    /// **No verification was performed.**  Covers every reason the check
    /// could not run:
    ///
    ///   * the signature-algorithm OID is not recognised at all;
    ///   * the algorithm is recognised but the key type / curve is not
    ///     carried by this build (P-521, Brainpool, explicit
    ///     `ECParameters`, DSA with a digest other than SHA-1/256, or a
    ///     key/algorithm family mismatch);
    ///   * the `SubjectPublicKeyInfo` would not parse;
    ///   * the crypto backend **rejected the key** — notably an RSA
    ///     modulus above `RsaPublicKey::MAX_SIZE` (4096 bits), so a
    ///     *legitimate* 8192-bit signer key lands here rather than being
    ///     mis-reported as a bad signature;
    ///   * the signature *encoding* is malformed (wrong length for the
    ///     modulus, un-decodable DER `SEQUENCE { r, s }`).
    ///
    /// (RSA PKCS#1 v1.5 SHA-1/256/384/512, ECDSA P-256/P-384, and DSA
    /// SHA-1/256 are all really verified — see module docs.)  Treated as
    /// **not trusted** by every caller, and never conflated with
    /// [`SigVerify::Bad`].
    Unsupported,
}

/// Parse a `SubjectPublicKeyInfo` SEQUENCE and recover the public key.
///
/// ```text
/// SubjectPublicKeyInfo ::= SEQUENCE {
///     algorithm        AlgorithmIdentifier,
///     subjectPublicKey BIT STRING }
/// ```
fn parse_spki(spki_der: &[u8]) -> Result<PublicKey, &'static str> {
    let spki = read_seq_strict(spki_der)?;
    let mut c = Cursor::new(&spki);
    // The AlgorithmIdentifier carries the key-type OID and, for EC keys,
    // the named-curve parameter OID; keep the whole SEQUENCE content.
    let alg_seq = c.read_seq_raw()?;
    let alg_oid = first_oid_of_seq(&alg_seq)?;
    let (bitstr, _) = c.read_tlv()?;
    if bitstr.tag != 0x03 {
        return Err("SPKI: subjectPublicKey is not BIT STRING");
    }
    if bitstr.content.is_empty() {
        return Err("SPKI: empty subjectPublicKey BIT STRING");
    }
    // Strip the leading "unused bits" octet (always 0 for whole-byte keys).
    let key_bits = &bitstr.content[1..];

    match alg_oid.as_str() {
        // rsaEncryption
        "1.2.840.113549.1.1.1" => {
            // RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }
            let rsa_seq = read_seq_strict(key_bits)?;
            let mut rc = Cursor::new(&rsa_seq);
            let n_bytes = rc.read_integer()?;
            let e_bytes = rc.read_integer()?;
            // INTEGER content is big-endian two's complement; for positive
            // values jarsigner-issued keys it may carry a single 0x00 sign
            // pad — `from_bytes_be` handles leading zeros fine.
            let n = BigUint::from_bytes_be(&n_bytes);
            let e = BigUint::from_bytes_be(&e_bytes);
            if n.is_zero() || e.is_zero() {
                return Err("SPKI: degenerate RSA key");
            }
            let k = (n.bit_length() + 7) / 8;
            Ok(PublicKey::Rsa(RsaPublicKey { n, e, k }))
        }
        // FEAT(jar-signer): id-ecPublicKey — dispatch on the named-curve
        // parameter OID inside the AlgorithmIdentifier.
        "1.2.840.10045.2.1" => match ec_named_curve_oid(&alg_seq) {
            // prime256v1 / secp256r1 (NIST P-256)
            Some(ref o) if o == "1.2.840.10045.3.1.7" => Ok(PublicKey::EcP256(spki_der.to_vec())),
            // secp384r1 (NIST P-384)
            Some(ref o) if o == "1.3.132.0.34" => Ok(PublicKey::EcP384(spki_der.to_vec())),
            // Any other / absent curve — unsupported, fail-closed.
            _ => Ok(PublicKey::Other),
        },
        // FEAT(jar-signer): id-dsa key OID (1.2.840.10040.4.1).
        "1.2.840.10040.4.1" => Ok(PublicKey::Dsa(spki_der.to_vec())),
        _ => Ok(PublicKey::Other),
    }
}

/// Extract the named-curve OID from an `id-ecPublicKey` AlgorithmIdentifier
/// SEQUENCE content (`SEQUENCE { algorithm OID, namedCurve OID }`).  Returns
/// `None` if the parameter is absent or is not an OID (e.g. `implicitCurve`
/// or explicit `ECParameters` — both of which we do not support).
fn ec_named_curve_oid(alg_seq_content: &[u8]) -> Option<String> {
    let mut c = Cursor::new(alg_seq_content);
    let _alg = c.read_oid().ok()?; // id-ecPublicKey
    c.read_oid().ok()
}

/// The public-key signature family named by a `signatureAlgorithm` OID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigFamily {
    Rsa,
    Ecdsa,
    Dsa,
}

/// Map a signature-algorithm OID to `(digest, family)`.  Returns `None`
/// for an OID we do not recognise at all (fail-closed upstream).
///
/// FEAT(jar-signer): extended to cover the ECDSA and DSA combined OIDs.
fn sig_alg_digest(oid: &str) -> Option<(DigestAlg, SigFamily)> {
    match oid {
        // RSA PKCS#1 v1.5
        "1.2.840.113549.1.1.5" => Some((DigestAlg::Sha1, SigFamily::Rsa)), // sha1WithRSA
        "1.2.840.113549.1.1.11" => Some((DigestAlg::Sha256, SigFamily::Rsa)), // sha256WithRSA
        "1.2.840.113549.1.1.12" => Some((DigestAlg::Sha384, SigFamily::Rsa)), // sha384WithRSA
        "1.2.840.113549.1.1.13" => Some((DigestAlg::Sha512, SigFamily::Rsa)), // sha512WithRSA
        // ECDSA
        "1.2.840.10045.4.1" => Some((DigestAlg::Sha1, SigFamily::Ecdsa)), // ecdsa-with-SHA1
        "1.2.840.10045.4.3.2" => Some((DigestAlg::Sha256, SigFamily::Ecdsa)), // ecdsa-with-SHA256
        "1.2.840.10045.4.3.3" => Some((DigestAlg::Sha384, SigFamily::Ecdsa)), // ecdsa-with-SHA384
        "1.2.840.10045.4.3.4" => Some((DigestAlg::Sha512, SigFamily::Ecdsa)), // ecdsa-with-SHA512
        // DSA (DSS).  id-dsa-with-sha1 (1.2.840.10040.4.3) and the NIST
        // SHA-256 DSA OID (2.16.840.1.101.3.4.3.2).
        "1.2.840.10040.4.3" => Some((DigestAlg::Sha1, SigFamily::Dsa)), // id-dsa-with-sha1
        "2.16.840.1.101.3.4.3.2" => Some((DigestAlg::Sha256, SigFamily::Dsa)), // id-dsa-with-sha256
        _ => None,
    }
}

/// Normalise a SignerInfo `signatureAlgorithm` OID.  When it is a bare
/// key-algorithm identifier (`rsaEncryption` / `id-ecPublicKey`) the
/// digest is implied by the SignerInfo's `digestAlgorithm`; map the pair
/// to the combined `<digest>With<key>` OID our verifier recognises.
/// Returns `None` (caller keeps the original OID) for already-combined
/// algorithms.
fn normalize_signer_sig_alg(sig_oid: &str, digest: DigestAlg) -> Option<String> {
    match sig_oid {
        // rsaEncryption — combine with the digest.
        "1.2.840.113549.1.1.1" => Some(
            match digest {
                DigestAlg::Sha1 => "1.2.840.113549.1.1.5",
                DigestAlg::Sha256 => "1.2.840.113549.1.1.11",
                DigestAlg::Sha384 => "1.2.840.113549.1.1.12",
                DigestAlg::Sha512 => "1.2.840.113549.1.1.13",
            }
            .to_string(),
        ),
        // id-ecPublicKey — combine with the digest.  FEAT(jar-signer):
        // now resolves to a verifiable ECDSA combined OID.
        "1.2.840.10045.2.1" => Some(
            match digest {
                DigestAlg::Sha1 => "1.2.840.10045.4.1",
                DigestAlg::Sha256 => "1.2.840.10045.4.3.2",
                DigestAlg::Sha384 => "1.2.840.10045.4.3.3",
                DigestAlg::Sha512 => "1.2.840.10045.4.3.4",
            }
            .to_string(),
        ),
        // FEAT(jar-signer): bare id-dsa key OID — combine with the digest.
        "1.2.840.10040.4.1" => Some(
            match digest {
                DigestAlg::Sha256 => "2.16.840.1.101.3.4.3.2",
                // SHA-1 (or anything the DSA path doesn't special-case)
                // maps to id-dsa-with-sha1.
                _ => "1.2.840.10040.4.3",
            }
            .to_string(),
        ),
        _ => None,
    }
}

/// Raw digest for an algorithm (covers all four SHA variants used by
/// PKCS#1 v1.5 / `.SF` bindings).  Returns `None` only for the impossible
/// case where the family is unimplemented.
fn raw_digest(alg: DigestAlg, data: &[u8]) -> Vec<u8> {
    match alg {
        DigestAlg::Sha1 => sha1::digest(data).to_vec(),
        DigestAlg::Sha256 => sha256::digest(data).to_vec(),
        DigestAlg::Sha384 => sha2ext::sha384(data).to_vec(),
        DigestAlg::Sha512 => sha2ext::sha512(data).to_vec(),
    }
}

/// DER `DigestInfo` prefix (the AlgorithmIdentifier + OCTET STRING header)
/// for each SHA variant, per PKCS#1 v1.5 (RFC 8017 §9.2).
fn digest_info_prefix(alg: DigestAlg) -> &'static [u8] {
    match alg {
        DigestAlg::Sha1 => &[
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
            0x14,
        ],
        DigestAlg::Sha256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ],
        DigestAlg::Sha384 => &[
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x02, 0x05, 0x00, 0x04, 0x30,
        ],
        DigestAlg::Sha512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x03, 0x05, 0x00, 0x04, 0x40,
        ],
    }
}

/// RSA PKCS#1 v1.5 verify: `signature^e mod n` must equal the expected
/// `EM = 0x00 || 0x01 || PS || 0x00 || DigestInfo(H(message))`.
///
/// # TRUST BOUNDARY: this proves possession of a key, not trust in it
///
/// [`SigVerify::Ok`] means exactly one thing: *these bytes are a valid
/// PKCS#1 v1.5 signature over this message under this public key*.  It
/// says nothing about whose key it is.  Trust is established (or not) by
/// [`verify_chain`], never here.
///
/// # Why this calls the `_checked` form
///
/// The shared kernel also exposes a `bool` wrapper
/// (`verify_rsa_pkcs1_v15`).  That wrapper is fail-closed but **ambiguous**:
/// it returns `false` both when the padded digest genuinely did not match
/// *and* when the backend refused the key outright — including for an
/// entirely legitimate signer whose modulus exceeds `RsaPublicKey::MAX_SIZE`
/// (4096 bits).  On a trust path that is the difference between "this is a
/// forgery" and "we never checked", and this site used to report the second
/// as the first.  `verify_rsa_pkcs1_v15_checked` keeps them apart:
///
///   * `Ok(true)`  → [`SigVerify::Ok`]          — verified.
///   * `Ok(false)` → [`SigVerify::Bad`]         — **preserved negative**:
///     the RSA operation ran and the digest did not match.
///   * `Err(_)`    → [`SigVerify::Unsupported`] — the key was rejected
///     (`InvalidKeyException`) or the signature encoding was malformed
///     (`SignatureException`).  Nothing was verified, so this is not a
///     negative security decision and must never be reported as one.
///
/// Both refusals fail closed; the migration changes the *diagnosis*, not
/// the accept/reject outcome.  See `docs/security/signed-jar-trust.md` §2.
fn rsa_pkcs1v15_verify(
    key: &RsaPublicKey,
    digest_alg: DigestAlg,
    message: &[u8],
    signature: &[u8],
) -> SigVerify {
    use cratonvm_native_builtins_crypto::signature::{
        verify_rsa_pkcs1_v15_checked, DigestAlgorithm,
    };
    let digest = match digest_alg {
        DigestAlg::Sha1 => DigestAlgorithm::Sha1,
        DigestAlg::Sha256 => DigestAlgorithm::Sha256,
        DigestAlg::Sha384 => DigestAlgorithm::Sha384,
        DigestAlg::Sha512 => DigestAlgorithm::Sha512,
    };
    let exponent_len = (key.e.bit_length() + 7) / 8;
    match verify_rsa_pkcs1_v15_checked(
        &key.n.to_bytes_be_padded(key.k),
        &key.e.to_bytes_be_padded(exponent_len),
        digest,
        message,
        signature,
    ) {
        Ok(true) => SigVerify::Ok,
        // PRESERVED NEGATIVE: the verification ran to completion and said no.
        Ok(false) => SigVerify::Bad,
        Err(e) => {
            // Not a verdict on the signature — a refusal to form one. Logged
            // so an over-large-but-legitimate signer key is diagnosable
            // instead of looking like a tampered JAR.
            warn!(
                "jar signer: RSA verification could not be performed ({}) — \
                 treating as unverifiable, NOT as a bad signature",
                e
            );
            SigVerify::Unsupported
        }
    }
}

/// Verify a signature `sig` over `message` using the public key encoded in
/// `signer_spki_der`, where `sig_alg_oid` names the signature algorithm.
///
/// FEAT(jar-signer): RSA PKCS#1 v1.5 (SHA-1/256/384/512), ECDSA P-256/P-384
/// (SHA-256/384/512), and DSA (SHA-1/256) are all fully verified.
///
/// # TRUST BOUNDARY: proves key possession, not signer identity
///
/// A [`SigVerify::Ok`] here proves only that `sig` is a valid signature
/// over `message` under the key in `signer_spki_der`.  Whether that key
/// belongs to a party this VM trusts is decided exclusively by
/// [`verify_chain`].
///
/// # Result mapping (three-valued — see [`SigVerify`])
///
/// * [`SigVerify::Bad`] — and **only** — when a verifier actually ran and
///   rejected the signature bytes.  That is the shape of a forgery.
/// * [`SigVerify::Unsupported`] whenever no verification could be
///   performed: unrecognised algorithm OID, an SPKI that will not parse, a
///   key type / curve this build does not carry, a key the backend refused,
///   or a malformed signature encoding.
fn verify_signature_with_spki(
    signer_spki_der: &[u8],
    sig_alg_oid: &str,
    message: &[u8],
    sig: &[u8],
) -> SigVerify {
    let (digest_alg, family) = match sig_alg_digest(sig_alg_oid) {
        Some(v) => v,
        None => return SigVerify::Unsupported,
    };
    // An SPKI we cannot decode is a key we never used — no verification
    // happened, so this is `Unsupported`, not a negative verdict.
    let key = match parse_spki(signer_spki_der) {
        Ok(k) => k,
        Err(e) => {
            warn!("jar signer: signer SubjectPublicKeyInfo unusable: {}", e);
            return SigVerify::Unsupported;
        }
    };
    match (key, family) {
        (PublicKey::Rsa(rsa), SigFamily::Rsa) => {
            rsa_pkcs1v15_verify(&rsa, digest_alg, message, sig)
        }
        (PublicKey::EcP256(spki), SigFamily::Ecdsa) => {
            ecdsa_p256_verify(&spki, digest_alg, message, sig)
        }
        (PublicKey::EcP384(spki), SigFamily::Ecdsa) => {
            ecdsa_p384_verify(&spki, digest_alg, message, sig)
        }
        (PublicKey::Dsa(spki), SigFamily::Dsa) => dsa_verify(&spki, digest_alg, message, sig),
        // Recognised algorithm but a key type / curve we do not handle
        // (P-521, Brainpool, key/alg mismatch, ...).  Fail-closed.
        _ => SigVerify::Unsupported,
    }
}

// ---------------------------------------------------------------------------
// FEAT(jar-signer): ECDSA verification (P-256 / P-384) via RustCrypto.
//
// The wire signature in both CMS SignerInfo and X.509 `signatureValue` is
// the DER `Ecdsa-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER }` form,
// which `ecdsa::Signature::from_der` parses.  The verifying key is decoded
// from the full `SubjectPublicKeyInfo` DER (the RustCrypto decoder rejects
// a point that is not on the curve, giving us point-validation for free).
//
// `VerifyingKey::verify_prehash` takes the raw message *digest*; we compute
// it with the digest the signature OID names.  This matches X.509 / CMS
// semantics exactly (ECDSA signs H(tbs) / H(SignedAttributes)).
// ---------------------------------------------------------------------------

/// A key that will not decode (including an off-curve point) and a
/// signature that is not a decodable DER `SEQUENCE { r, s }` both mean the
/// ECDSA verification never ran — [`SigVerify::Unsupported`].  Only
/// `verify_prehash` saying "no" is a [`SigVerify::Bad`] verdict.
fn ecdsa_p256_verify(
    spki_der: &[u8],
    digest_alg: DigestAlg,
    message: &[u8],
    sig: &[u8],
) -> SigVerify {
    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    use p256::pkcs8::DecodePublicKey;

    let vk = match VerifyingKey::from_public_key_der(spki_der) {
        Ok(k) => k,
        Err(_) => return SigVerify::Unsupported,
    };
    let signature = match Signature::from_der(sig) {
        Ok(s) => s,
        Err(_) => return SigVerify::Unsupported,
    };
    let prehash = raw_digest(digest_alg, message);
    match vk.verify_prehash(&prehash, &signature) {
        Ok(()) => SigVerify::Ok,
        Err(_) => SigVerify::Bad,
    }
}

fn ecdsa_p384_verify(
    spki_der: &[u8],
    digest_alg: DigestAlg,
    message: &[u8],
    sig: &[u8],
) -> SigVerify {
    use p384::ecdsa::signature::hazmat::PrehashVerifier;
    use p384::ecdsa::{Signature, VerifyingKey};
    use p384::pkcs8::DecodePublicKey;

    // See `ecdsa_p256_verify`: undecodable key or signature ⇒ nothing was
    // checked ⇒ `Unsupported`, never `Bad`.
    let vk = match VerifyingKey::from_public_key_der(spki_der) {
        Ok(k) => k,
        Err(_) => return SigVerify::Unsupported,
    };
    let signature = match Signature::from_der(sig) {
        Ok(s) => s,
        Err(_) => return SigVerify::Unsupported,
    };
    let prehash = raw_digest(digest_alg, message);
    match vk.verify_prehash(&prehash, &signature) {
        Ok(()) => SigVerify::Ok,
        Err(_) => SigVerify::Bad,
    }
}

// ---------------------------------------------------------------------------
// FEAT(jar-signer): DSA (DSS) verification via the RustCrypto `dsa` crate.
//
// The `*.DSA` signer block / X.509 link carries a DER
// `Dss-Sig-Value ::= SEQUENCE { r INTEGER, s INTEGER }` signature.  The
// `dsa::VerifyingKey` is decoded from the SPKI DER (it recovers `(p,q,g)`
// and `y`).  DSA verifies over the raw message digest, so we hash with the
// OID-named digest (SHA-1 or SHA-256) and call `verify_prehash`.
// ---------------------------------------------------------------------------

fn dsa_verify(spki_der: &[u8], digest_alg: DigestAlg, message: &[u8], sig: &[u8]) -> SigVerify {
    use dsa::pkcs8::DecodePublicKey;
    use dsa::signature::hazmat::PrehashVerifier;
    use dsa::{Signature, VerifyingKey};

    // DSA in JARs uses SHA-1 or SHA-256 only; reject anything else.
    if !matches!(digest_alg, DigestAlg::Sha1 | DigestAlg::Sha256) {
        return SigVerify::Unsupported;
    }
    // See `ecdsa_p256_verify`: an undecodable `(p, q, g, y)` or a signature
    // that is not a DER `SEQUENCE { r, s }` means no verification ran.
    let vk = match VerifyingKey::from_public_key_der(spki_der) {
        Ok(k) => k,
        Err(_) => return SigVerify::Unsupported,
    };
    // `Signature` decodes from the DER `SEQUENCE { r, s }` via `TryFrom<&[u8]>`.
    let signature = match Signature::try_from(sig) {
        Ok(s) => s,
        Err(_) => return SigVerify::Unsupported,
    };
    let prehash = raw_digest(digest_alg, message);
    match vk.verify_prehash(&prehash, &signature) {
        Ok(()) => SigVerify::Ok,
        Err(_) => SigVerify::Bad,
    }
}

// ---------------------------------------------------------------------------
// Task #40 — Trust store + chain verification
// ---------------------------------------------------------------------------
//
// HotSpot / OpenJDK chains a JAR signer's leaf cert to a trust anchor in
// `jssecacerts` / `cacerts` before treating the signer as authoritative.
// Without that step `Class.getCodeSource().getCertificates()` will trust
// a self-signed leaf, defeating any `policy.signedBy(<distinguishedName>)`
// rule.  This module adds:
//
//   * A [`TrustStore`] of in-memory X.509 anchor certificates.
//   * A small X.509 parser ([`X509Cert`]) — Subject / Issuer DN plus the
//     TBSCertificate / signature blobs needed for chain walking.  We use
//     the local TLV reader (`read_tlv`, `Cursor`) — no new deps.
//   * [`verify_chain`], which walks leaf → intermediates → root by
//     matching each cert's `issuer` DN against the next cert's `subject`
//     DN, then asks [`X509Cert::link_signature_ok`] whether the
//     `signature` blob ties the child to the parent.
//
// **Cryptographic note.**  The link-signature step is a real public-key
// verify against the parent cert's public key — see
// [`verify_signature_with_spki`].  RSA PKCS#1 v1.5 (SHA-1/256/384/512),
// ECDSA-with-SHA-1/256/384/512 on NIST P-256 / P-384, and DSA (DSS) with
// SHA-1 / SHA-256 are all fully verified (the EC/DSA paths use the
// RustCrypto `p256` / `p384` / `dsa` crates, which also re-validate the
// public point against its curve).  Only genuinely-unsupported
// combinations (P-521 / Brainpool / explicit ECParameters, DSA with an
// unusual digest, key/alg mismatch) surface as
// [`TrustError::NotImplemented`], which [`verify_signer_block`] surfaces
// as `None` — keeping the walk fail-closed for those residual cases.
// The synthetic `craton-stub-sig` algorithm OID is retained ONLY for the
// in-process test fixtures.

use std::sync::OnceLock;

/// Synthetic signature algorithm OID used by the in-process test fixtures
/// for [`verify_chain`].  Never appears in real X.509 certificates.
///
/// The "signature" is computed as `SHA-256(TBSCertificate || issuer_subject_dn)`
/// where `issuer_subject_dn` is the raw DER bytes of the parent cert's
/// Subject Name.  Tampering with either the TBS bytes (test #3) or the
/// purported parent breaks the equality and the chain step rejects.
///
/// OID is in Craton's private arc (1.3.6.1.4.1.0.55 — RFC 5280 §4.1.2.7).
pub const OID_STUB_SIG: &str = "1.3.6.1.4.1.0.55";

/// Reasons [`verify_chain`] can refuse a leaf certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustError {
    /// We walked leaf → intermediates → ??? but no parent in the
    /// `intermediates` list nor in the trust store matched the current
    /// cert's `issuer` DN.
    NoTrustAnchor,
    /// We found a parent by DN but its signature does not cover the
    /// child's TBSCertificate (broken-intermediate path; test #3).
    BadSignature,
    /// The chain looped back onto a cert we'd already visited (defence
    /// against poisoned input that injects a cycle of self-signed
    /// intermediates).
    Cyclic,
    /// The chain is deeper than [`MAX_CHAIN_LEN`].
    TooLong,
    /// The signature on a chain link uses an algorithm/key combination
    /// this build cannot verify.  RSA PKCS#1 v1.5, ECDSA P-256/P-384, and
    /// DSA (SHA-1/256) links are all verified; only genuinely-unsupported
    /// cases land here (EC curves we do not carry — P-521 / Brainpool /
    /// explicit ECParameters — DSA with an unusual digest, or a key/alg
    /// mismatch).  See module-level docs.
    NotImplemented,
    /// One of the certs is structurally invalid (wrong tag, truncated
    /// TBSCertificate, missing Subject/Issuer, ...).
    Malformed,
    /// A cert in the chain is outside its `[notBefore, notAfter]`
    /// validity window relative to the current wall-clock time.
    Expired,
    /// FEAT(jar-signer): RFC 5280 §6.1.4 — a CA cert in the path asserts
    /// `BasicConstraints.cA = false`, or its KeyUsage lacks `keyCertSign`,
    /// or the `pathLenConstraint` was exceeded.
    BasicConstraintsViolation,
    /// FEAT(jar-signer): RFC 5280 — the leaf's KeyUsage forbids
    /// `digitalSignature`, or its ExtendedKeyUsage does not permit
    /// code signing.
    KeyUsageViolation,
    /// FEAT(jar-signer): RFC 5280 §6.1.4(f) — a cert carries a *critical*
    /// extension this validator does not recognise / process.
    UnknownCriticalExtension,
}

/// Hard cap on chain depth.  Real-world TLS / code-signing chains run
/// 3-4 deep.  Cap at 16 to bound the recursive walk against an attacker
/// who chains many self-signed intermediates trying to exhaust stack.
pub const MAX_CHAIN_LEN: usize = 16;

/// Maximum number of anchors a single [`TrustStore`] will hold.  The
/// JDK `cacerts` ships ~150 anchors; 4096 is well above any realistic
/// deployment and protects against a malicious PEM bundle.
pub const MAX_TRUST_ANCHORS: usize = 4096;

/// In-memory set of trusted root anchors.
///
/// # Sources (priority order, populated by [`TrustStore::load_default`])
///
/// 1. `javax.net.ssl.trustStore` system property (read from the env-var
///    `JAVAX_NET_SSL_TRUSTSTORE`, password from
///    `JAVAX_NET_SSL_TRUSTSTOREPASSWORD`).  Format auto-detected: PEM,
///    JKS (in-module walker, MAC-verified), or PKCS#12 (`p12` crate,
///    MAC-verified).
/// 2. `CRATONVM_TRUST_PEM` env-var pointing at a PEM bundle.
/// 3. `rustls-native-certs`-style system root store — integration seam.
///    `classloading` cannot pull the crate (it lives in `native-builtins`,
///    which depends on `classloading`); the host VM calls
///    [`TrustStore::extend_from_anchors`] with the decoded DER blobs.
/// 4. The JDK `cacerts` path — `$JAVA_HOME/lib/security/cacerts` (JKS,
///    password `changeit` unless overridden).  Now parsed natively: every
///    TrustedCertEntry becomes a trust anchor.
///
/// An empty trust store rejects every chain with
/// [`TrustError::NoTrustAnchor`].  `TrustStore::permissive_legacy_tests`
/// (`#[cfg(test)]`) exists only for the pre-task-#40 self-consistency
/// fixtures in this file.
#[derive(Debug, Default, Clone)]
pub struct TrustStore {
    anchors: Vec<X509Anchor>,
    /// When `true`, [`verify_signer_block`] skips the chain step
    /// entirely.  Used **only** by the legacy self-consistency tests.
    permissive_legacy: bool,
    /// Free-form list of source descriptors, for diagnostics.
    pub sources_loaded: Vec<String>,
}

/// Materialised trust anchor: the cert's Subject DN bytes + its raw DER.
#[derive(Debug, Clone)]
struct X509Anchor {
    /// Bytes of the `subject` Name TLV (the full SEQUENCE OF RDN), used
    /// for `issuer == anchor.subject` comparison.
    subject_dn: Vec<u8>,
    /// Full DER of the anchor cert (kept so callers can re-display).
    der: Vec<u8>,
}

impl TrustStore {
    /// Construct an empty trust store.  Equivalent to `TrustStore::default()`.
    pub fn empty() -> Self {
        TrustStore::default()
    }

    /// Construct a trust store that skips the chain step in
    /// [`verify_signer_block`].  **Tests only.**
    ///
    /// # Why this is `#[cfg(test)]`
    ///
    /// The `permissive_legacy` flag disables *two* independent checks at
    /// once: the SignerInfo public-key signature (`let enforce_pubkey =
    /// !trust_store.permissive_legacy`) and the whole chain walk.  A
    /// `TrustStore` built by this constructor therefore accepts any signer
    /// block whose `.SF` digest is self-consistent — no signature math, no
    /// trust anchor, no expiry.  As a plain `pub fn` it was a live
    /// fail-open switch reachable from every crate that depends on
    /// `cratonvm_classloading`, one call site away from silently turning
    /// JAR signature verification off in production.  Gating the
    /// *constructor* (not the flag, which the two branches still read)
    /// means a non-test build simply has no way to produce a permissive
    /// store: `TrustStore::default()` / `empty()` / `load_default()` all
    /// leave `permissive_legacy == false`.
    #[cfg(test)]
    pub fn permissive_legacy_tests() -> Self {
        let mut t = Self::default();
        t.permissive_legacy = true;
        t.sources_loaded.push("permissive-legacy-tests".to_string());
        t
    }

    /// Number of anchors currently loaded.
    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    /// Append a single trust anchor from raw X.509 DER.  Returns `false`
    /// if the cert won't parse (malformed) or the store is already full.
    pub fn add_anchor_der(&mut self, der: Vec<u8>) -> bool {
        if self.anchors.len() >= MAX_TRUST_ANCHORS {
            warn!(
                "trust store: refusing to add anchor — MAX_TRUST_ANCHORS ({}) reached",
                MAX_TRUST_ANCHORS
            );
            return false;
        }
        let parsed = match X509Cert::parse(&der) {
            Ok(c) => c,
            Err(e) => {
                warn!("trust store: anchor rejected — malformed X.509: {}", e);
                return false;
            }
        };
        self.anchors.push(X509Anchor {
            subject_dn: parsed.subject_dn.to_vec(),
            der,
        });
        true
    }

    /// Bulk-extend the trust store from pre-decoded DER blobs.  This is
    /// the integration seam for callers that already have a system root
    /// store decoded (e.g. via `rustls_native_certs::load_native_certs()`
    /// in the host VM).  Returns the number of anchors successfully added.
    pub fn extend_from_anchors<I>(&mut self, ders: I) -> usize
    where
        I: IntoIterator<Item = Vec<u8>>,
    {
        let mut n = 0;
        for der in ders {
            if self.add_anchor_der(der) {
                n += 1;
            }
        }
        n
    }

    /// Append every PEM-formatted CERTIFICATE block found in `pem_text`.
    /// Other PEM types (`RSA PRIVATE KEY`, `EC PRIVATE KEY`, ...) are
    /// ignored.  Returns the number of anchors added.
    ///
    /// PEM lines must use LF (`\n`) or CRLF — both are accepted.  We
    /// recognise both `-----BEGIN CERTIFICATE-----` and the legacy
    /// `-----BEGIN TRUSTED CERTIFICATE-----` headers.
    pub fn load_pem_bundle(&mut self, pem_text: &str) -> usize {
        let mut added = 0;
        let mut in_block = false;
        let mut accum = String::new();
        for line in pem_text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("-----BEGIN")
                && (trimmed.contains("CERTIFICATE") || trimmed.contains("TRUSTED CERTIFICATE"))
            {
                in_block = true;
                accum.clear();
                continue;
            }
            if trimmed.starts_with("-----END") {
                if in_block {
                    if let Some(der) = base64_decode(&accum) {
                        if self.add_anchor_der(der) {
                            added += 1;
                        }
                    } else {
                        warn!("trust store: base64 decode failed inside PEM block");
                    }
                }
                in_block = false;
                accum.clear();
                continue;
            }
            if in_block {
                accum.push_str(trimmed);
            }
        }
        added
    }

    /// Populate a trust store from the standard JCE / CratonVM sources
    /// listed at the top of this module.  Failures on any individual
    /// source are logged and skipped — the only way the function
    /// returns an empty store is if every source is absent or
    /// unparseable.
    pub fn load_default() -> Self {
        let mut ts = TrustStore::default();

        // 1. javax.net.ssl.trustStore.  Java sees this as a system
        // property; we read it from the env-var spelling the VM emits
        // when materialising sysprops back to native code.  The matching
        // password sysprop is `javax.net.ssl.trustStorePassword`.
        if let Ok(p) = cratonvm_types::flags::runtime_var("JAVAX_NET_SSL_TRUSTSTORE") {
            let pw = cratonvm_types::flags::runtime_var("JAVAX_NET_SSL_TRUSTSTOREPASSWORD")
                .unwrap_or_else(|_| "changeit".to_string());
            ts.try_load_path(&p, "javax.net.ssl.trustStore", &pw);
        }

        // 2. CratonVM-native PEM bundle (no password).
        if let Some(p) = crate::loader_flags().trust_pem.as_deref() {
            ts.try_load_path(p, "CRATONVM_TRUST_PEM", "");
        }

        // 3. System root store — integration seam.  `rustls-native-certs`
        // lives in `native-builtins` (not reachable here without a crate
        // cycle); the host VM, which already links it, calls
        // `extend_from_anchors` directly with the decoded DER blobs.  We
        // record that the seam exists.
        ts.sources_loaded.push(
            "system-root-store: extend_from_anchors() seam (host VM supplies DER)".to_string(),
        );

        // 4. JDK `cacerts` fallback (JKS, password "changeit").  Now
        // parsed natively by the in-module JKS walker — every
        // TrustedCertEntry becomes a trust anchor.  Honour
        // `JAVAX_NET_SSL_TRUSTSTOREPASSWORD` if set; otherwise the JDK
        // default "changeit".
        if let Some(jh) = cratonvm_types::flags::runtime_var_os("JAVA_HOME") {
            let mut path = std::path::PathBuf::from(jh);
            path.push("lib");
            path.push("security");
            path.push("cacerts");
            if path.exists() {
                let pw = cratonvm_types::flags::runtime_var("JAVAX_NET_SSL_TRUSTSTOREPASSWORD")
                    .unwrap_or_else(|_| "changeit".to_string());
                ts.try_load_path(&path.to_string_lossy(), "JDK cacerts", &pw);
            }
        }
        ts
    }

    /// Attempt to load one on-disk source, auto-detecting the format from
    /// the leading bytes:
    ///
    ///   * `-----BEGIN` banner            → PEM bundle.
    ///   * `0xFEEDFEED` magic             → JKS keystore (in-module walker).
    ///   * leading `0x30` (DER SEQUENCE)  → PKCS#12 / PFX (`p12` crate).
    ///
    /// Every trust-anchor / cert entry found is added via
    /// [`Self::add_anchor_der`].  Failures are logged and recorded in
    /// `sources_loaded`; they never panic and never poison the store.
    ///
    /// `password` is the keystore integrity / decryption password (JKS MAC
    /// and PKCS#12 MAC).  PEM bundles ignore it.
    fn try_load_path(&mut self, p: &str, label: &str, password: &str) {
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                warn!("{}={} could not be read: {}", label, p, e);
                return;
            }
        };

        // PEM banner takes priority (a PEM bundle never starts with 0x30
        // or the JKS magic).
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if text.contains("-----BEGIN") {
                let n = self.load_pem_bundle(text);
                self.sources_loaded
                    .push(format!("{}={} (PEM, {} anchors)", label, p, n));
                return;
            }
        }

        if bytes.starts_with(&[0xFE, 0xED, 0xFE, 0xED]) {
            match parse_jks_trusted_certs(&bytes, password.as_bytes()) {
                Ok(ders) => {
                    let n = self.extend_from_anchors(ders);
                    self.sources_loaded
                        .push(format!("{}={} (JKS, {} anchors)", label, p, n));
                }
                Err(e) => {
                    warn!("{}={} JKS parse failed: {}", label, p, e);
                    self.sources_loaded
                        .push(format!("{}={} JKS: parse error ({})", label, p, e));
                }
            }
            return;
        }

        if bytes.first() == Some(&TAG_SEQUENCE) {
            match parse_pkcs12_certs(&bytes, password) {
                Ok(ders) => {
                    let n = self.extend_from_anchors(ders);
                    self.sources_loaded
                        .push(format!("{}={} (PKCS#12, {} anchors)", label, p, n));
                }
                Err(e) => {
                    warn!("{}={} PKCS#12 parse failed: {}", label, p, e);
                    self.sources_loaded
                        .push(format!("{}={} PKCS#12: parse error ({})", label, p, e));
                }
            }
            return;
        }

        warn!(
            "{}={} is not a recognised trust-store format (no PEM banner, JKS magic, or DER SEQUENCE)",
            label, p
        );
        self.sources_loaded
            .push(format!("{}={} unrecognised format", label, p));
    }

    /// Look up an anchor whose Subject DN equals `dn`.
    fn find_anchor_by_subject(&self, dn: &[u8]) -> Option<&X509Anchor> {
        self.anchors.iter().find(|a| a.subject_dn.as_slice() == dn)
    }
}

// ---------------------------------------------------------------------------
// JKS trust-store parsing
// ---------------------------------------------------------------------------
//
// The JKS binary format is fully open (mirrors
// `cratonvm-native-builtins::keystore::load_jks`, which we cannot import
// without a crate cycle).  Layout:
//
//   u32 magic = 0xFEEDFEED
//   u32 version (1 or 2)
//   u32 entry_count
//   entry_count * {
//     u32 tag (1 = PrivateKeyEntry, 2 = TrustedCertEntry)
//     u16 alias_len + UTF-8 alias
//     u64 creation_date_ms
//     match tag {
//       1 => { u32 enc_key_len + key; u32 chain_count;
//              chain_count * { u16 cert_type_len + type; u32 der_len + der } }
//       2 => { u16 cert_type_len + type; u32 der_len + der }
//     }
//   }
//   [SHA1(password_utf16be || "Mighty Aphrodite" || body)]   // 20-byte tag
//
// We verify the trailing 20-byte integrity tag FIRST (fail-closed: a
// tampered cacerts is rejected outright), then collect every X.509 DER —
// both TrustedCertEntry certs and the certs in PrivateKeyEntry chains —
// as candidate trust anchors.

const JKS_MAGIC: u32 = 0xFEED_FEED;
const JKS_HMAC_SALT: &[u8] = b"Mighty Aphrodite";

/// JKS integrity tag: `SHA1(password_utf16be || "Mighty Aphrodite" || body)`.
/// Empty password → empty UTF-16 prefix.  Each password byte is treated as
/// a Latin-1 codepoint emitted as UTF-16BE (high byte 0) — matches the JDK
/// for the common ASCII case.
fn jks_password_mac(password_bytes: &[u8], body: &[u8]) -> [u8; 20] {
    let mut buf = Vec::with_capacity(password_bytes.len() * 2 + JKS_HMAC_SALT.len() + body.len());
    for &b in password_bytes {
        buf.push(0u8);
        buf.push(b);
    }
    buf.extend_from_slice(JKS_HMAC_SALT);
    buf.extend_from_slice(body);
    sha1::digest(&buf)
}

struct JksReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> JksReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        JksReader { data, pos: 0 }
    }
    fn need(&self, n: usize) -> Result<(), &'static str> {
        if self.pos + n > self.data.len() {
            Err("JKS: truncated")
        } else {
            Ok(())
        }
    }
    fn u16_be(&mut self) -> Result<u16, &'static str> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }
    fn u32_be(&mut self) -> Result<u32, &'static str> {
        self.need(4)?;
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }
    fn skip(&mut self, n: usize) -> Result<(), &'static str> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }
    fn skip_u16len(&mut self) -> Result<(), &'static str> {
        let n = self.u16_be()? as usize;
        self.skip(n)
    }
    fn bytes_u32len(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u32_be()? as usize;
        self.need(n)?;
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn skip_u32len(&mut self) -> Result<(), &'static str> {
        let n = self.u32_be()? as usize;
        self.skip(n)
    }
}

/// Parse a JKS keystore and return every embedded X.509 certificate DER
/// (trusted certs + chain certs).  Verifies the integrity MAC first.
fn parse_jks_trusted_certs(bytes: &[u8], password: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    if bytes.len() < 4 + 4 + 4 + 20 {
        return Err("JKS: too short");
    }
    // Integrity tag covers everything before the trailing 20 bytes.
    let body_end = bytes.len() - 20;
    let stored = &bytes[body_end..];
    let body = &bytes[..body_end];
    let computed = jks_password_mac(password, body);
    if !ct_eq(stored, &computed) {
        return Err("JKS: integrity MAC mismatch (wrong password or tampered store)");
    }

    let mut r = JksReader::new(bytes);
    if r.u32_be()? != JKS_MAGIC {
        return Err("JKS: bad magic");
    }
    let version = r.u32_be()?;
    if version != 1 && version != 2 {
        return Err("JKS: unsupported version");
    }
    let entry_count = r.u32_be()? as usize;
    // Bound entry_count against the file size (each entry is >= 14 bytes).
    if entry_count > bytes.len() {
        return Err("JKS: implausible entry count");
    }

    let mut out: Vec<Vec<u8>> = Vec::new();
    for _ in 0..entry_count {
        let tag = r.u32_be()?;
        r.skip_u16len()?; // alias
        r.skip(8)?; // creation_date_ms (u64)
        match tag {
            1 => {
                // PrivateKeyEntry: encrypted key, then a cert chain.
                r.skip_u32len()?; // enc key
                let chain_count = r.u32_be()? as usize;
                if chain_count > bytes.len() {
                    return Err("JKS: implausible chain count");
                }
                for _ in 0..chain_count {
                    r.skip_u16len()?; // cert type ("X.509")
                    let der = r.bytes_u32len()?;
                    if der.len() <= MAX_CERT_DER {
                        out.push(der.to_vec());
                    }
                }
            }
            2 => {
                // TrustedCertEntry.  Both v1 and v2 prefix the cert with a
                // cert-type string in real OpenJDK; read it unconditionally.
                r.skip_u16len()?; // cert type
                let der = r.bytes_u32len()?;
                if der.len() <= MAX_CERT_DER {
                    out.push(der.to_vec());
                }
            }
            _ => return Err("JKS: unknown entry tag"),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PKCS#12 / PFX trust-store parsing — backed by the `p12` crate.
// ---------------------------------------------------------------------------

/// Parse a PKCS#12 / PFX file and return every X.509 CertBag DER.  Verifies
/// the PFX MAC against `password` first (fail-closed).
fn parse_pkcs12_certs(bytes: &[u8], password: &str) -> Result<Vec<Vec<u8>>, &'static str> {
    let pfx = p12::PFX::parse(bytes).map_err(|_| "PKCS#12: parse failed")?;
    if !pfx.verify_mac(password) {
        return Err("PKCS#12: MAC verification failed (wrong password or tampered store)");
    }
    let bags = pfx
        .bags(password)
        .map_err(|_| "PKCS#12: bag decode failed")?;
    let mut out = Vec::new();
    for bag in &bags {
        if let p12::SafeBagKind::CertBag(p12::CertBag::X509(der)) = &bag.bag {
            if der.len() <= MAX_CERT_DER {
                out.push(der.clone());
            }
        }
    }
    Ok(out)
}

/// Process-wide default trust store, lazily populated on first call.
///
/// `class_path.rs::extract_jar_signer_blocks` reaches for this; tests
/// in this file build a private one instead.
///
/// # TRUST BOUNDARY: an empty store is the strict outcome, not a broken one
///
/// [`TrustStore::load_default`] logs and skips every source it cannot
/// read, so on a host with no `JAVA_HOME/lib/security/cacerts`, no
/// `javax.net.ssl.trustStore` and no `CRATONVM_TRUST_PEM` this returns a
/// store with **zero anchors**.  That is deliberate and safe: with no
/// anchor, [`verify_chain`] bottoms out in [`TrustError::NoTrustAnchor`]
/// and every JAR is reported unsigned.  It is never "trust everything".
///
/// The consequence to be aware of is availability, not security: signed
/// JARs stop being *recognised* as signed rather than starting to be
/// wrongly trusted.  Inspect `TrustStore::sources_loaded` to tell the two
/// situations apart.
pub fn default_trust_store() -> &'static TrustStore {
    static TS: OnceLock<TrustStore> = OnceLock::new();
    TS.get_or_init(TrustStore::load_default)
}

/// Minimal X.509 certificate view used by chain validation.
///
/// We intentionally implement only the fields chain validation needs:
///
/// ```text
/// Certificate ::= SEQUENCE {
///     tbsCertificate       TBSCertificate,
///     signatureAlgorithm   AlgorithmIdentifier,
///     signatureValue       BIT STRING
/// }
/// TBSCertificate ::= SEQUENCE {
///     [0] EXPLICIT version DEFAULT v1,
///     serialNumber          CertificateSerialNumber,
///     signature             AlgorithmIdentifier,
///     issuer                Name,
///     validity              Validity,
///     subject               Name,
///     subjectPublicKeyInfo  SubjectPublicKeyInfo,
///     ...
/// }
/// ```
#[derive(Debug, Clone)]
pub struct X509Cert<'a> {
    /// Raw TBSCertificate bytes (the to-be-signed envelope — fed into
    /// the signature check on the parent link).
    pub tbs_der: &'a [u8],
    /// `issuer` Name DER bytes (the *entire* SEQUENCE TLV).
    pub issuer_dn: &'a [u8],
    /// `subject` Name DER bytes (the entire SEQUENCE TLV).
    pub subject_dn: &'a [u8],
    /// Algorithm OID of the outer `signatureAlgorithm`.
    pub sig_alg_oid: String,
    /// The `signatureValue` content (BIT STRING content, minus the
    /// leading unused-bits byte).
    pub signature_bytes: &'a [u8],
    /// Raw `subjectPublicKeyInfo` SEQUENCE bytes (entire TLV).  Fed to
    /// [`parse_spki`] when this cert acts as the *parent* in a chain link
    /// or as the signer whose key verifies the SignerInfo signature.
    pub spki_der: &'a [u8],
    /// `notBefore` time bytes (the raw UTCTime / GeneralizedTime content,
    /// tag stripped).  Empty if unparsed.
    pub not_before: &'a [u8],
    /// `notBefore` ASN.1 tag (0x17 UTCTime / 0x18 GeneralizedTime).
    pub not_before_tag: u8,
    /// `notAfter` time bytes (content, tag stripped).
    pub not_after: &'a [u8],
    /// `notAfter` ASN.1 tag.
    pub not_after_tag: u8,
    /// FEAT(jar-signer): the complete Certificate DER (entire outer
    /// SEQUENCE), kept so RFC 5280 extension processing can re-parse the
    /// cert with the `x509-cert` crate.
    pub full_der: &'a [u8],
}

impl<'a> X509Cert<'a> {
    /// Parse a complete X.509 Certificate DER.
    pub fn parse(der: &'a [u8]) -> Result<Self, &'static str> {
        let (outer, total) = read_tlv(der)?;
        if outer.tag != TAG_SEQUENCE {
            return Err("X509Cert: outer is not SEQUENCE");
        }
        if total != der.len() {
            return Err("X509Cert: trailing bytes after outer SEQUENCE");
        }
        let body = outer.content;
        // TBSCertificate (SEQUENCE).
        let (tbs_tlv, tbs_total) = read_tlv(body)?;
        if tbs_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: TBSCertificate is not SEQUENCE");
        }
        let tbs_der_full = &body[..tbs_total];
        let tbs_body = tbs_tlv.content;

        // Walk TBSCertificate to extract issuer and subject Name TLVs.
        // First field may be [0] EXPLICIT version (DEFAULT v1).  Skip
        // if present.
        let mut rest = tbs_body;
        if let Some(&first) = rest.first() {
            if first == TAG_CTX0 {
                let (_, n) = read_tlv(rest)?;
                rest = &rest[n..];
            }
        }
        // serialNumber INTEGER.
        let (_, n) = read_tlv(rest)?;
        rest = &rest[n..];
        // signature AlgorithmIdentifier (SEQUENCE).
        let (_, n) = read_tlv(rest)?;
        rest = &rest[n..];
        // issuer Name (SEQUENCE OF RDN).
        let issuer_start = rest;
        let (issuer_tlv, n) = read_tlv(rest)?;
        if issuer_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: issuer is not SEQUENCE");
        }
        let issuer_dn = &issuer_start[..n];
        rest = &rest[n..];
        // validity (SEQUENCE { notBefore Time, notAfter Time }).
        let (validity_tlv, n) = read_tlv(rest)?;
        if validity_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: validity is not SEQUENCE");
        }
        let (mut not_before, mut not_before_tag) = (&b""[..], 0u8);
        let (mut not_after, mut not_after_tag) = (&b""[..], 0u8);
        {
            let v = validity_tlv.content;
            if let Ok((nb, nb_total)) = read_tlv(v) {
                not_before = nb.content;
                not_before_tag = nb.tag;
                if let Ok((na, _)) = read_tlv(&v[nb_total..]) {
                    not_after = na.content;
                    not_after_tag = na.tag;
                }
            }
        }
        rest = &rest[n..];
        // subject Name (SEQUENCE OF RDN).
        let subject_start = rest;
        let (subj_tlv, n) = read_tlv(rest)?;
        if subj_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: subject is not SEQUENCE");
        }
        let subject_dn = &subject_start[..n];
        rest = &rest[n..];

        // subjectPublicKeyInfo (SEQUENCE).
        let spki_start = rest;
        let (spki_tlv, n) = read_tlv(rest)?;
        if spki_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: subjectPublicKeyInfo is not SEQUENCE");
        }
        let spki_der = &spki_start[..n];

        // Outer signatureAlgorithm + signatureValue.
        let outer_rest = &body[tbs_total..];
        let (sig_alg_tlv, sig_alg_total) = read_tlv(outer_rest)?;
        if sig_alg_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: signatureAlgorithm is not SEQUENCE");
        }
        let sig_alg_oid = first_oid_of_seq(sig_alg_tlv.content)?;
        let sig_value_buf = &outer_rest[sig_alg_total..];
        let (sig_tlv, _) = read_tlv(sig_value_buf)?;
        if sig_tlv.tag != 0x03 {
            return Err("X509Cert: signatureValue is not BIT STRING");
        }
        // BIT STRING content starts with an "unused bits" byte.  Skip it.
        if sig_tlv.content.is_empty() {
            return Err("X509Cert: empty BIT STRING in signatureValue");
        }
        let signature_bytes = &sig_tlv.content[1..];

        Ok(X509Cert {
            tbs_der: tbs_der_full,
            issuer_dn,
            subject_dn,
            sig_alg_oid,
            signature_bytes,
            spki_der,
            not_before,
            not_before_tag,
            not_after,
            not_after_tag,
            full_der: &der[..total],
        })
    }

    /// Is this cert self-signed (Subject DN == Issuer DN)?
    pub fn is_self_signed(&self) -> bool {
        self.subject_dn == self.issuer_dn
    }

    /// Does `self`'s signature link it to `parent`?  Verifies that
    /// `parent`'s public key signs `self.tbs_der`, yielding
    /// `self.signature_bytes`.
    ///
    /// * Real RSA PKCS#1 v1.5 (SHA-1/256/384/512) is fully verified
    ///   against `parent.spki_der`.
    /// * Real ECDSA-with-SHA-1/256/384/512 on NIST P-256 / P-384 and DSA
    ///   (DSS) with SHA-1 / SHA-256 are fully verified against
    ///   `parent.spki_der` via the RustCrypto `p256` / `p384` / `dsa`
    ///   verifiers (see [`verify_signature_with_spki`]).  The EC point /
    ///   curve-membership check is performed by the verifying-key decoder.
    /// * Only genuinely-unsupported combinations — EC curves we do not
    ///   carry (P-521, Brainpool, explicit `ECParameters`), DSA with a
    ///   non-SHA-1/256 digest, or any other key/algorithm mismatch —
    ///   surface as [`TrustError::NotImplemented`] (fail-closed).
    /// * The synthetic [`OID_STUB_SIG`] acceptance path exists ONLY under
    ///   `cfg(test)` (it is a test backdoor — see the security note in the
    ///   body).  In a production build it is compiled out, so an
    ///   `OID_STUB_SIG` cert is an unknown algorithm and surfaces as
    ///   [`TrustError::NotImplemented`] (fail-closed).
    pub fn link_signature_ok(&self, parent: &X509Cert) -> Result<(), TrustError> {
        // SECURITY (cert-chain signature bypass): the synthetic
        // [`OID_STUB_SIG`] acceptance path is an in-process TEST backdoor —
        // it accepts a link on `signature == SHA-256(tbs || parent.subject_dn)`
        // with NO real-crypto verification and NO trust-store consultation.
        // `sig_alg_oid` is an attacker-controllable certificate field, so in a
        // production build this branch would let a forged chain validate.  It
        // is therefore compiled out of non-test builds entirely: a real binary
        // never contains this code.  Under `cfg(test)` it remains available for
        // the in-crate X.509-shaped fixtures (see the `tests` module).  In
        // production an `OID_STUB_SIG` cert is an unknown algorithm and falls
        // through to `verify_signature_with_spki`, which returns `Unsupported`
        // → `NotImplemented` (fail-closed).
        #[cfg(test)]
        if self.sig_alg_oid == OID_STUB_SIG {
            // Test-only computation: SHA-256(tbs_der || parent.subject_dn).
            let mut buf = Vec::with_capacity(self.tbs_der.len() + parent.subject_dn.len());
            buf.extend_from_slice(self.tbs_der);
            buf.extend_from_slice(parent.subject_dn);
            let expected = sha256::digest(&buf);
            return if ct_eq(self.signature_bytes, &expected) {
                Ok(())
            } else {
                Err(TrustError::BadSignature)
            };
        }
        // TRUST BOUNDARY: this proves ONE link (parent's key signs self's
        // TBSCertificate).  It proves nothing about the parent being an
        // anchor, about validity windows, or about extensions — those are
        // [`verify_chain`]'s job.  The three-valued result is kept distinct
        // all the way out: `Bad` = a verifier ran and said no;
        // `NotImplemented` = no verification was performed at all.  Both
        // refuse, and neither can be mistaken for the other by a caller
        // matching on `TrustError`.
        match verify_signature_with_spki(
            parent.spki_der,
            &self.sig_alg_oid,
            self.tbs_der,
            self.signature_bytes,
        ) {
            SigVerify::Ok => Ok(()),
            SigVerify::Bad => Err(TrustError::BadSignature),
            SigVerify::Unsupported => Err(TrustError::NotImplemented),
        }
    }
}

/// Parse a UTCTime / GeneralizedTime into a comparable `YYYYMMDDHHMMSS`
/// 14-byte numeric string (best-effort).  Returns `None` if the value is
/// not in a recognised form.  UTCTime years 00-49 → 2000-2049, 50-99 →
/// 1950-1999 (RFC 5280 §4.1.2.5.1).
fn parse_asn1_time(tag: u8, content: &[u8]) -> Option<[u8; 14]> {
    // Accept only the canonical "...Z" UTC encodings emitted by every CA.
    let s = std::str::from_utf8(content).ok()?;
    let s = s.strip_suffix('Z').unwrap_or(s);
    let digits: Vec<u8> = s.bytes().filter(|b| b.is_ascii_digit()).collect();
    let mut out = [b'0'; 14];
    match tag {
        0x17 => {
            // UTCTime: YYMMDDHHMM[SS]
            if digits.len() < 10 {
                return None;
            }
            let yy: u32 = std::str::from_utf8(&digits[0..2]).ok()?.parse().ok()?;
            let century = if yy < 50 { b"20" } else { b"19" };
            out[0] = century[0];
            out[1] = century[1];
            // Copy YYMMDDHHMM (+ optional SS) after the century.
            let take = digits.len().min(12);
            out[2..2 + take].copy_from_slice(&digits[..take]);
            Some(out)
        }
        0x18 => {
            // GeneralizedTime: YYYYMMDDHHMM[SS]
            if digits.len() < 12 {
                return None;
            }
            let take = digits.len().min(14);
            out[..take].copy_from_slice(&digits[..take]);
            Some(out)
        }
        _ => None,
    }
}

/// Current UTC time as a `YYYYMMDDHHMMSS` 14-byte numeric string, for
/// comparison against parsed cert validity windows.
fn now_utc_14() -> [u8; 14] {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days (Howard Hinnant's algorithm) — no chrono dep.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let mut out = [0u8; 14];
    let s = format!("{:04}{:02}{:02}{:02}{:02}{:02}", year, m, d, hh, mm, ss);
    let b = s.as_bytes();
    out[..b.len().min(14)].copy_from_slice(&b[..b.len().min(14)]);
    out
}

/// Check that `now` falls within `[notBefore, notAfter]`.
///
/// # TRUST BOUNDARY: fail-closed on an unreadable validity window
///
/// A bound we cannot parse means the validity check **did not happen**.
/// This function previously returned `true` in that case, which recorded
/// "in date" for a cert whose dates were never examined — the same
/// did-not-check-reads-as-checked conflation the three-valued
/// [`SigVerify`] exists to prevent, one level up.  It now refuses.
///
/// RFC 5280 §4.1.2.5 admits exactly two encodings — `UTCTime` (tag 0x17)
/// and `GeneralizedTime` (tag 0x18) — and [`parse_asn1_time`] accepts
/// both, so a conforming certificate always parses.  What is rejected is a
/// cert with a missing, truncated, or non-`Time` validity field, which
/// [`X509Cert::parse`] tolerates structurally (it leaves the tag `0` and
/// the bytes empty) but which no CA emits.
///
/// The synthetic `OID_STUB_SIG` fixtures are exempted by the callers via
/// [`is_stub_sig_fixture`], which is hard-`false` in production builds.
fn cert_dates_ok(cert: &X509Cert) -> bool {
    let now = now_utc_14();
    match parse_asn1_time(cert.not_before_tag, cert.not_before) {
        Some(nb) => {
            if now < nb {
                return false;
            }
        }
        None => {
            warn!("jar signer: cert notBefore is not a readable ASN.1 Time — refusing");
            return false;
        }
    }
    match parse_asn1_time(cert.not_after_tag, cert.not_after) {
        Some(na) => {
            if now > na {
                return false;
            }
        }
        None => {
            warn!("jar signer: cert notAfter is not a readable ASN.1 Time — refusing");
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// FEAT(jar-signer): RFC 5280 extension processing.
//
// We parse the certificate's extensions with the audited `x509-cert` crate
// and enforce the subset relevant to JAR code-signing path validation:
//
//   * BasicConstraints (§4.2.1.9) — a cert used to certify another cert
//     MUST be a CA (`cA = TRUE`); `pathLenConstraint` bounds the number of
//     intervening non-self-issued CA certs below it.
//   * KeyUsage (§4.2.1.3) — a CA cert MUST assert `keyCertSign`; a leaf
//     (end-entity) MUST assert `digitalSignature` (or `nonRepudiation`).
//   * ExtendedKeyUsage (§4.2.1.12) — a leaf carrying an EKU MUST permit
//     `id-kp-codeSigning` (or `anyExtendedKeyUsage`).
//   * Unknown *critical* extensions (§6.1.4(f)) — reject (fail-closed).
//
// Posture for *absent* extensions: enforce-if-present.  RFC 5280 requires
// conforming CAs to carry BasicConstraints/KeyUsage, but real legacy roots
// (and self-signed code-signing certs) often omit them, and HotSpot's
// jarsigner accepts such certs.  We therefore do NOT hard-reject a cert
// for *lacking* an extension; we only reject when a *present* extension
// forbids the role the cert is being used in.  Unknown-critical-extension
// rejection still applies regardless.
// ---------------------------------------------------------------------------

/// OID text constants (rendered dotted-decimal) for the extensions we
/// recognise as "processed".  Any *critical* extension whose OID is not in
/// this set causes [`TrustError::UnknownCriticalExtension`].
const OID_EXT_BASIC_CONSTRAINTS: &str = "2.5.29.19";
const OID_EXT_KEY_USAGE: &str = "2.5.29.15";
const OID_EXT_EXT_KEY_USAGE: &str = "2.5.29.37";
const OID_EXT_SUBJECT_KEY_ID: &str = "2.5.29.14";
const OID_EXT_AUTHORITY_KEY_ID: &str = "2.5.29.35";
const OID_EXT_SUBJECT_ALT_NAME: &str = "2.5.29.17";
const OID_EXT_AUTHORITY_INFO_ACCESS: &str = "1.3.6.1.5.5.7.1.1";
const OID_KP_CODE_SIGNING: &str = "1.3.6.1.5.5.7.3.3";
const OID_ANY_EXT_KEY_USAGE: &str = "2.5.29.37.0";

/// Parsed RFC 5280 extension facts for one certificate.
#[derive(Default, Debug)]
struct CertExtFacts {
    /// `Some(is_ca)` if a BasicConstraints extension was present.
    basic_ca: Option<bool>,
    /// `pathLenConstraint`, if present.
    path_len: Option<u8>,
    /// `Some(())` if a KeyUsage extension was present, with the two bits
    /// we care about.
    key_usage_present: bool,
    ku_digital_signature: bool,
    ku_key_cert_sign: bool,
    /// `Some(true)` if an ExtendedKeyUsage extension permits code signing
    /// (codeSigning or anyExtendedKeyUsage); `Some(false)` if an EKU is
    /// present but does not; `None` if no EKU extension at all.
    eku_allows_code_signing: Option<bool>,
}

/// Parse `cert_der` with `x509-cert` and extract the RFC 5280 facts we
/// enforce.  Returns `Err(UnknownCriticalExtension)` if the cert carries a
/// critical extension we do not process; `Err(Malformed)` if it will not
/// decode at all.  A cert with no extensions yields all-`None` facts.
fn extract_ext_facts(cert_der: &[u8]) -> Result<CertExtFacts, TrustError> {
    use der::Decode;
    use x509_cert::ext::pkix::{BasicConstraints, ExtendedKeyUsage, KeyUsage};
    use x509_cert::Certificate;

    let cert = Certificate::from_der(cert_der).map_err(|_| TrustError::Malformed)?;
    let mut facts = CertExtFacts::default();

    let exts = match &cert.tbs_certificate.extensions {
        Some(e) => e,
        None => return Ok(facts), // v1/v2 cert or no extensions.
    };

    // First pass: reject any unrecognised *critical* extension (§6.1.4(f)).
    for ext in exts.iter() {
        if !ext.critical {
            continue;
        }
        let oid = ext.extn_id.to_string();
        let recognised = matches!(
            oid.as_str(),
            OID_EXT_BASIC_CONSTRAINTS
                | OID_EXT_KEY_USAGE
                | OID_EXT_EXT_KEY_USAGE
                | OID_EXT_SUBJECT_KEY_ID
                | OID_EXT_AUTHORITY_KEY_ID
                | OID_EXT_SUBJECT_ALT_NAME
                | OID_EXT_AUTHORITY_INFO_ACCESS
        );
        if !recognised {
            warn!(
                "jar signer: cert carries unprocessed critical extension {}",
                oid
            );
            return Err(TrustError::UnknownCriticalExtension);
        }
    }

    // BasicConstraints.
    if let Ok(Some((_crit, bc))) = exts_get::<BasicConstraints>(&cert) {
        facts.basic_ca = Some(bc.ca);
        facts.path_len = bc.path_len_constraint;
    }
    // KeyUsage.
    if let Ok(Some((_crit, ku))) = exts_get::<KeyUsage>(&cert) {
        facts.key_usage_present = true;
        facts.ku_digital_signature = ku.digital_signature() || ku.non_repudiation();
        facts.ku_key_cert_sign = ku.key_cert_sign();
    }
    // ExtendedKeyUsage.
    if let Ok(Some((_crit, eku))) = exts_get::<ExtendedKeyUsage>(&cert) {
        let allows = eku.0.iter().any(|o| {
            let s = o.to_string();
            s == OID_KP_CODE_SIGNING || s == OID_ANY_EXT_KEY_USAGE
        });
        facts.eku_allows_code_signing = Some(allows);
    }

    Ok(facts)
}

/// Helper: typed extension lookup that swallows the (already-handled)
/// decode errors into `Ok(None)`.  Delegates to `x509-cert`'s
/// `TbsCertificate::get::<T>()`, which decodes the single matching
/// extension by its `AssociatedOid`.
fn exts_get<'a, T>(cert: &'a x509_cert::Certificate) -> Result<Option<(bool, T)>, ()>
where
    T: der::Decode<'a> + const_oid::AssociatedOid,
{
    cert.tbs_certificate.get::<T>().map_err(|_| ())
}

/// Enforce RFC 5280 constraints on the **leaf** (end-entity) certificate.
fn check_leaf_ext_facts(facts: &CertExtFacts) -> Result<(), TrustError> {
    // If a KeyUsage extension is present it MUST allow digitalSignature
    // (or nonRepudiation) — a leaf used to sign a JAR.
    if facts.key_usage_present && !facts.ku_digital_signature {
        warn!("jar signer: leaf KeyUsage forbids digitalSignature");
        return Err(TrustError::KeyUsageViolation);
    }
    // If an ExtendedKeyUsage is present it MUST permit code signing.
    if let Some(false) = facts.eku_allows_code_signing {
        warn!("jar signer: leaf ExtendedKeyUsage does not permit codeSigning");
        return Err(TrustError::KeyUsageViolation);
    }
    Ok(())
}

/// Enforce RFC 5280 constraints on a certificate being used as a **CA**
/// (an intermediate or the trust anchor) to certify the cert below it.
/// `ca_certs_below` is the count of non-self-issued CA certs already seen
/// below this one in the path, used for `pathLenConstraint` checking.
fn check_ca_ext_facts(facts: &CertExtFacts, ca_certs_below: usize) -> Result<(), TrustError> {
    // If BasicConstraints is present, cA MUST be TRUE.
    if let Some(false) = facts.basic_ca {
        warn!("jar signer: CA cert has BasicConstraints cA=FALSE");
        return Err(TrustError::BasicConstraintsViolation);
    }
    // pathLenConstraint: max number of non-self-issued intermediate CAs
    // that may follow below this cert in the path.
    if let Some(max) = facts.path_len {
        if ca_certs_below > max as usize {
            warn!(
                "jar signer: pathLenConstraint {} exceeded ({} CAs below)",
                max, ca_certs_below
            );
            return Err(TrustError::BasicConstraintsViolation);
        }
    }
    // If KeyUsage is present it MUST allow keyCertSign.
    if facts.key_usage_present && !facts.ku_key_cert_sign {
        warn!("jar signer: CA cert KeyUsage forbids keyCertSign");
        return Err(TrustError::BasicConstraintsViolation);
    }
    Ok(())
}

/// SECURITY (cert-chain signature bypass): is `cert` a synthetic
/// [`OID_STUB_SIG`] test fixture whose dummy validity dates / missing
/// extensions should be skipped?
///
/// In a **production** build this is hard-wired to `false` so the
/// `OID_STUB_SIG` skip-branches in [`verify_chain`] (which would otherwise
/// bypass `cert_dates_ok` and the RFC 5280 extension checks on the strength
/// of the attacker-controllable `signatureAlgorithm` OID) can never be
/// taken — date and extension validation always apply.  The stub recognition
/// only exists under `cfg(test)` for the in-crate fixtures.
#[cfg(test)]
#[inline]
fn is_stub_sig_fixture(cert: &X509Cert) -> bool {
    cert.sig_alg_oid == OID_STUB_SIG
}

/// Production stub: `OID_STUB_SIG` fixtures do not exist outside tests, so
/// every cert is treated as a real cert and gets full date / extension
/// validation.  See the `cfg(test)` variant above for the rationale.
#[cfg(not(test))]
#[inline]
fn is_stub_sig_fixture(_cert: &X509Cert) -> bool {
    false
}

/// Walk `leaf` → `intermediates` → `trust_store` building a chain.
///
/// At each step the current cert's `issuer` DN is looked up first
/// against the supplied intermediates, then against the trust store.
/// The first match consumes that parent; the parent then becomes the
/// new "current" cert.  Termination conditions:
///
///   * Anchor reached — `Ok(())`.
///   * No parent for current cert — `Err(NoTrustAnchor)`.
///   * Bad link signature — `Err(BadSignature)`.  RSA, ECDSA P-256/P-384,
///     and DSA (SHA-1/256) links are all cryptographically verified.
///   * Unsupported curve/key/digest combination (P-521, Brainpool,
///     explicit ECParameters, unusual DSA digest, key/alg mismatch) —
///     `Err(NotImplemented)`.
///   * Chain exceeds [`MAX_CHAIN_LEN`] — `Err(TooLong)`.
///   * Already-visited cert — `Err(Cyclic)`.
///   * RFC 5280 extension violation — `Err(BasicConstraintsViolation)` /
///     `Err(KeyUsageViolation)` / `Err(UnknownCriticalExtension)`.
pub fn verify_chain<'a>(
    leaf: &'a X509Cert<'a>,
    intermediates: &'a [X509Cert<'a>],
    trust_store: &TrustStore,
) -> Result<(), TrustError> {
    verify_chain_path(leaf, intermediates, trust_store).map(|_| ())
}

/// Exactly [`verify_chain`], but on success returns **the certificates that
/// actually formed the validated path**, leaf first and anchor last.
///
/// # TRUST BOUNDARY: only these certs were validated
///
/// A CMS `certificates` set is attacker-supplied and may carry any number
/// of certificates that took no part in path construction.  [`verify_chain`]
/// silently ignores them, which is correct for the accept/reject decision
/// but leaves the caller holding a bag of certs of which only some were
/// checked.  Surfacing that whole bag as
/// `Class.getCodeSource().getCertificates()` would let an attacker append a
/// certificate of their choosing to an otherwise legitimately signed JAR
/// and have it reported as one of the signer's certificates — enough to
/// fool a policy `signedBy` filter that scans the array.
///
/// This function therefore returns the path itself, so the caller can
/// report *only* what was proven.  Every element has had its signature
/// verified against its issuer, its validity window checked, and its RFC
/// 5280 extensions enforced for the role it played; the final element is a
/// trust anchor.  Certificates present in the input but absent from the
/// returned path are, by construction, unverified.
///
/// What is still **not** proven for the returned path: revocation status
/// (no CRL/OCSP), name constraints, and certificate policies — see
/// `docs/security/signed-jar-trust.md` §4.
pub fn verify_chain_path<'a>(
    leaf: &'a X509Cert<'a>,
    intermediates: &'a [X509Cert<'a>],
    trust_store: &TrustStore,
) -> Result<Vec<Vec<u8>>, TrustError> {
    let mut current: &'a X509Cert<'a> = leaf;
    // The validated path, leaf first. Nothing is appended before the step
    // that validates it, so a `?` early-return can never leave an
    // unverified cert in the result.
    let mut path: Vec<Vec<u8>> = vec![leaf.full_der.to_vec()];
    // PERF(cl-jarsigner-perf): track visited Subject DNs in a `HashSet` of
    // borrowed `&'a [u8]` slices for O(1) cycle-detection membership instead
    // of the former `Vec<Vec<u8>>` + linear `iter().any(...)` scan per step.
    // The leaf and every intermediate `subject_dn` are `&'a [u8]`, so no
    // owned copies (`.to_vec()`) are needed — this also drops the per-step
    // DN allocation. Behavior is unchanged: the set holds exactly the same
    // Subject DNs that were pushed before, and `insert`-returns-false ⇔ the
    // old `any(...)` would have matched.
    let mut visited: std::collections::HashSet<&'a [u8]> = std::collections::HashSet::new();
    visited.insert(current.subject_dn);

    // PERF(cl-jarsigner-perf): index intermediates by Subject DN for O(1)
    // parent lookup instead of `intermediates.iter().find(...)` per step.
    // `entry(...).or_insert(...)` keeps the FIRST occurrence of any duplicate
    // Subject DN, exactly matching the old `find()` (which returned the first
    // matching cert in slice order). Lookups below preserve identical result.
    let mut inter_by_subject: std::collections::HashMap<&'a [u8], &'a X509Cert<'a>> =
        std::collections::HashMap::with_capacity(intermediates.len());
    for c in intermediates {
        inter_by_subject.entry(c.subject_dn).or_insert(c);
    }

    // Leaf validity window must include "now" (skip for the synthetic
    // stub-sig fixtures, whose dummy validity dates aren't real times).
    if !is_stub_sig_fixture(leaf) && !cert_dates_ok(leaf) {
        return Err(TrustError::Expired);
    }

    // FEAT(jar-signer): RFC 5280 leaf extension checks (KeyUsage /
    // ExtendedKeyUsage / unknown-critical).  Skipped for stub-sig
    // fixtures (which carry no extensions and aren't real certs) —
    // `is_stub_sig_fixture` is hard-`false` in production builds, so the
    // checks always run there.
    if !is_stub_sig_fixture(leaf) {
        let facts = extract_ext_facts(leaf.full_der)?;
        check_leaf_ext_facts(&facts)?;
    }

    // Number of (non-self-issued) CA certs encountered below the cert
    // currently being validated as a CA — drives pathLenConstraint.
    let mut ca_certs_below: usize = 0;

    for _step in 0..MAX_CHAIN_LEN {
        if let Some(anchor) = trust_store.find_anchor_by_subject(current.issuer_dn) {
            let parent = X509Cert::parse(&anchor.der).map_err(|_| TrustError::Malformed)?;
            if !is_stub_sig_fixture(&parent) && !cert_dates_ok(&parent) {
                return Err(TrustError::Expired);
            }
            // FEAT(jar-signer): the anchor certifies `current`, so it acts
            // as a CA — enforce BasicConstraints / KeyUsage on it.
            if !is_stub_sig_fixture(&parent) {
                let facts = extract_ext_facts(parent.full_der)?;
                check_ca_ext_facts(&facts, ca_certs_below)?;
            }
            current.link_signature_ok(&parent)?;
            // Anchor reached and the link to it verified — record it last.
            path.push(anchor.der.clone());
            return Ok(path);
        }
        // Look up an intermediate whose subject == current.issuer.
        // PERF(cl-jarsigner-perf): O(1) indexed lookup (was a linear
        // `iter().find(...)`); `.copied()` yields the same first-match cert.
        let next = inter_by_subject.get(current.issuer_dn).copied();
        match next {
            Some(parent) => {
                // PERF(cl-jarsigner-perf): O(1) set membership (was a linear
                // `visited.iter().any(...)`); same Cyclic result.
                if visited.contains(parent.subject_dn) {
                    return Err(TrustError::Cyclic);
                }
                if !is_stub_sig_fixture(parent) && !cert_dates_ok(parent) {
                    return Err(TrustError::Expired);
                }
                // FEAT(jar-signer): `parent` is an intermediate CA that
                // certifies `current` — enforce CA constraints, counting
                // the non-self-issued CAs already below it.
                if !is_stub_sig_fixture(parent) {
                    let facts = extract_ext_facts(parent.full_der)?;
                    check_ca_ext_facts(&facts, ca_certs_below)?;
                }
                current.link_signature_ok(parent)?;
                // Link verified, dates checked, CA constraints enforced —
                // only now does this intermediate join the validated path.
                path.push(parent.full_der.to_vec());
                // PERF(cl-jarsigner-perf): insert borrowed DN slice (no alloc).
                visited.insert(parent.subject_dn);
                // A non-self-issued intermediate adds to the CA count that
                // the *next* (higher) CA's pathLenConstraint must cover.
                if !parent.is_self_signed() {
                    ca_certs_below += 1;
                }
                current = parent;
            }
            None => {
                // Self-signed leaves land here too (issuer == subject
                // already in `visited`).  Either way no anchor.
                return Err(TrustError::NoTrustAnchor);
            }
        }
    }
    Err(TrustError::TooLong)
}

// ---------------------------------------------------------------------------
// Tiny base64 decoder for PEM bundles — avoids a `base64` dep.
// ---------------------------------------------------------------------------

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    // Strip whitespace.
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    let mut padding = 0usize;
    for &c in &cleaned {
        let v = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            _ => return None, // illegal char
        };
        if padding > 0 {
            return None; // non-pad after pad
        }
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// V1 — per-entry manifest digest binding (jarsigner integrity chain)
//
// `verify_signer_block` proves the signature over `MANIFEST.MF` (via the
// `.SF` companion).  That is only the first half of the JAR signing trust
// chain.  The full chain is:
//
//     signature  ->  .SF  ->  MANIFEST.MF  ->  per-entry digests  ->  bytes
//
// Without the second half an attacker can take a properly signed JAR,
// replace a `.class` body (leaving `MANIFEST.MF` / `.SF` / `.RSA` intact),
// and `Class.getCodeSource().getCertificates()` would still report the
// original signer.  The helpers below let the archive-holding caller
// (`class_path::extract_jar_signer_blocks`) bind every entry's bytes to
// the digest recorded in `MANIFEST.MF` before surfacing the signer.
// ---------------------------------------------------------------------------

/// One per-entry digest declaration parsed out of a JAR `MANIFEST.MF`
/// per-entry section: the entry's `Name:` and the strongest recognised
/// `<alg>-Digest:` value (decoded from base64) found in that section.
#[derive(Debug, Clone)]
pub struct ManifestEntryDigest {
    /// The `Name:` value — a JAR-internal entry path (e.g.
    /// `com/example/Foo.class`).
    pub name: String,
    /// Digest algorithm named by the `<alg>-Digest` attribute.
    pub alg: DigestAlg,
    /// The expected digest bytes (base64-decoded from the manifest).
    pub expected: Vec<u8>,
}

/// Map a JAR-manifest digest-attribute algorithm token (the `<alg>` in
/// `<alg>-Digest` / `<alg>-Digest-Manifest`) to a [`DigestAlg`].
///
/// The JAR spec spells SHA-1 as both `SHA1` and `SHA-1`; SHA-2 variants
/// always carry the dash (`SHA-256`).  Matching is case-insensitive.
fn manifest_digest_alg(token: &str) -> Option<DigestAlg> {
    match token.to_ascii_uppercase().as_str() {
        "SHA-256" | "SHA256" => Some(DigestAlg::Sha256),
        "SHA-384" | "SHA384" => Some(DigestAlg::Sha384),
        "SHA-512" | "SHA512" => Some(DigestAlg::Sha512),
        "SHA1" | "SHA-1" => Some(DigestAlg::Sha1),
        _ => None,
    }
}

/// Relative strength ordering for digest algorithms — higher is stronger.
/// Used to pick the strongest `<alg>-Digest` when a manifest section lists
/// several (jarsigner emits whichever the signer requested; if more than
/// one is present we verify against the strongest).
fn digest_strength(alg: DigestAlg) -> u8 {
    match alg {
        DigestAlg::Sha1 => 1,
        DigestAlg::Sha256 => 2,
        DigestAlg::Sha384 => 3,
        DigestAlg::Sha512 => 4,
    }
}

/// Fold a JAR manifest/`.SF` blob into logical lines, honouring the
/// 72-byte continuation rule (a physical line that begins with a single
/// space continues the previous logical line; the leading space is
/// dropped).  Mirrors the folding `ManifestInfo` performs in
/// `class_path.rs`, kept self-contained here so the signer module has no
/// dependency on the class-path manifest parser.
fn fold_manifest_lines(text: &str) -> Vec<String> {
    let text = text.replace('\r', "");
    let mut folded: Vec<String> = Vec::new();
    let mut buf = String::new();
    for line in text.split('\n') {
        if line.starts_with(' ') && !buf.is_empty() {
            buf.push_str(&line[1..]);
        } else {
            if !buf.is_empty() {
                folded.push(std::mem::take(&mut buf));
            }
            buf.push_str(line);
        }
    }
    if !buf.is_empty() {
        folded.push(buf);
    }
    folded
}

/// Verify that the caller-supplied `MANIFEST.MF` bytes are the same bytes
/// the verified `.SF` committed to, by matching the `.SF` main-section
/// `<alg>-Digest-Manifest` attribute against the freshly computed digest
/// of `manifest_bytes`.
///
/// This binds the (signature-verified) `.SF` to the actual `MANIFEST.MF`
/// content; it is the link that lets the per-entry digests in the manifest
/// be trusted.  Returns `true` only when a recognised
/// `<alg>-Digest-Manifest` attribute is present in the `.SF` main section
/// **and** its base64 value equals `H_alg(manifest_bytes)`.  An absent
/// `*-Digest-Manifest` returns `false` (fail-closed: we will not trust a
/// manifest the `.SF` did not commit to).
///
/// # TRUST BOUNDARY: this is an INTEGRITY check, not a trust check
///
/// A `true` proves one thing: *`manifest_bytes` hashes to the value
/// recorded in these `.SF` bytes.*  It is a digest comparison — no key, no
/// certificate, no anchor is involved, and this function does not know or
/// care whether the `.SF` it was handed was ever signed.  Feeding it a
/// `.SF` you wrote yourself yields `true`, correctly.
///
/// It becomes meaningful only in composition: the caller must **first**
/// have obtained a `Some(_)` from [`verify_signer_block`] for *these same*
/// `.SF` bytes.  That, and only that, turns "the manifest matches the
/// `.SF`" into "the manifest matches what a verified signer committed to".
pub fn verify_sf_binds_manifest(sf_bytes: &[u8], manifest_bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(sf_bytes);
    let lines = fold_manifest_lines(&text);
    let mut best: Option<(DigestAlg, Vec<u8>)> = None;
    for line in &lines {
        // Stop at the first blank line: only the `.SF` main section
        // carries `-Digest-Manifest`; per-entry sections follow.
        if line.is_empty() {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        // We match `<alg>-Digest-Manifest` (digest of the whole manifest).
        // `<alg>-Digest-Manifest-Main-Attributes` is intentionally NOT
        // accepted as a substitute — it only covers the main section, not
        // the per-entry digests we are about to trust.
        let Some(alg_token) = key.strip_suffix("-Digest-Manifest") else {
            continue;
        };
        let Some(alg) = manifest_digest_alg(alg_token) else {
            continue;
        };
        let Some(expected) = base64_decode(value.trim()) else {
            continue;
        };
        if best
            .as_ref()
            .map_or(true, |(b, _)| digest_strength(alg) > digest_strength(*b))
        {
            best = Some((alg, expected));
        }
    }
    match best {
        Some((alg, expected)) => {
            let actual = raw_digest(alg, manifest_bytes);
            !actual.is_empty() && ct_eq(&expected, &actual)
        }
        None => false,
    }
}

/// Parse every per-entry section of a JAR `MANIFEST.MF` into the strongest
/// `<alg>-Digest` declaration it carries.  Each manifest section is
/// `Name: <entry>` followed by one or more `<alg>-Digest: <base64>` lines;
/// the main section (before the first blank line, no `Name:`) is skipped.
///
/// The returned vector lists, per signed entry, the entry name, the
/// digest algorithm, and the expected digest bytes — the caller fetches
/// the entry's actual bytes and compares with [`digest_matches`].
///
/// # TRUST BOUNDARY: this is a PARSER, and the list is exhaustive
///
/// This function makes no security decision at all; it reports what the
/// manifest text says.  The security-relevant property is what it
/// *omits*: an archive entry with no `Name:` section, or a section
/// carrying no recognised `<alg>-Digest`, produces **no element**.  A
/// caller must therefore treat "absent from this list" as **unsigned** —
/// that is the classic partial-signing gap, where a `.class` injected into
/// a signed JAR but never named in the manifest would otherwise inherit
/// the signer's certificates.  Enumerating the archive and asking "is this
/// entry in the list?" is the check; the enforcement site is
/// `class_path::ClassPath::certs_for_signed_class`.
///
/// Directory sections (`Name:` ending in `/`) carry no digest and are
/// likewise absent — correctly, since a directory has no bytes to sign.
pub fn parse_manifest_entry_digests(manifest_bytes: &[u8]) -> Vec<ManifestEntryDigest> {
    let text = String::from_utf8_lossy(manifest_bytes);
    let lines = fold_manifest_lines(&text);
    let mut out: Vec<ManifestEntryDigest> = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_best: Option<(DigestAlg, Vec<u8>)> = None;

    // Flush the in-progress section (only if it has BOTH a name and a
    // digest); always clears the in-progress state. Plain `fn` (captures
    // nothing) to keep the borrows unambiguous.
    fn flush(
        name: &mut Option<String>,
        best: &mut Option<(DigestAlg, Vec<u8>)>,
        out: &mut Vec<ManifestEntryDigest>,
    ) {
        if let (Some(n), Some((alg, expected))) = (name.take(), best.take()) {
            out.push(ManifestEntryDigest {
                name: n,
                alg,
                expected,
            });
        } else {
            *name = None;
            *best = None;
        }
    }

    for line in &lines {
        if line.is_empty() {
            // Section boundary: flush whatever we have.
            flush(&mut cur_name, &mut cur_best, &mut out);
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.eq_ignore_ascii_case("Name") {
            // New section starts. (Defensive: a `Name` before the first
            // blank line would still be treated as a per-entry section.)
            flush(&mut cur_name, &mut cur_best, &mut out);
            // Cap entry-name length to keep a hostile manifest from
            // ballooning memory; legitimate JAR paths are well under 4 KiB.
            if value.len() <= 4096 {
                cur_name = Some(value.to_string());
            } else {
                cur_name = None;
            }
            cur_best = None;
        } else if let Some(alg_token) = key.strip_suffix("-Digest") {
            if cur_name.is_some() {
                if let Some(alg) = manifest_digest_alg(alg_token) {
                    if let Some(expected) = base64_decode(value) {
                        if cur_best
                            .as_ref()
                            .map_or(true, |(b, _)| digest_strength(alg) > digest_strength(*b))
                        {
                            cur_best = Some((alg, expected));
                        }
                    }
                }
            }
        }
    }
    // Flush the final section (manifests need not end with a blank line).
    flush(&mut cur_name, &mut cur_best, &mut out);
    out
}

/// Constant-time check that `H_alg(data)` equals the `expected` digest
/// recorded in a manifest entry section.  Returns `false` on length
/// mismatch (i.e. tampered or wrong-algorithm bytes).
///
/// # TRUST BOUNDARY: this is an INTEGRITY check, not a trust check
///
/// A `true` proves that `data` hashes to `expected` under `alg` — nothing
/// more.  `expected` is only worth anything if it came out of a manifest
/// that a verified signer committed to; supplied from anywhere else this
/// is a plain hash comparison.  There is no key and no certificate here.
///
/// Fail-closed in both directions: an `expected` of the wrong length —
/// including the empty slice, the classic success-shaped default — can
/// never match, because `ct_eq` compares lengths first.
pub fn digest_matches(alg: DigestAlg, data: &[u8], expected: &[u8]) -> bool {
    let actual = raw_digest(alg, data);
    !actual.is_empty() && ct_eq(expected, &actual)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // DER builder helpers — minimal encoder used by the test fixtures.
    // We deliberately re-implement these here (instead of pulling
    // `native-builtins::jca::asn1`) to keep the test self-contained and
    // independent of the rest of the workspace.
    // -----------------------------------------------------------------

    /// Task #40: trust-store-aware tests below build real X.509-shaped
    /// fixtures and supply a proper [`TrustStore`].  The older
    /// self-consistency-only tests in this module embed marker-shaped
    /// "certs" that won't parse as X.509 — they use this permissive
    /// store to keep covering the pre-chain code paths.
    fn legacy_ts() -> TrustStore {
        TrustStore::permissive_legacy_tests()
    }

    /// The `permissive_legacy` flag switches off BOTH the SignerInfo
    /// public-key check and the chain walk. Every constructor that a
    /// production build can reach must leave it `false` — the only one that
    /// sets it is `permissive_legacy_tests`, which is `#[cfg(test)]` and so
    /// does not exist outside this configuration.
    ///
    /// If a new non-test constructor ever sets the flag, this test fails and
    /// the regression is caught before it becomes a silent "signature
    /// verification is off in release" bug.
    #[test]
    fn production_trust_store_constructors_are_never_permissive() {
        assert!(
            !TrustStore::default().permissive_legacy,
            "TrustStore::default() must enforce the pubkey + chain checks"
        );
        assert!(
            !TrustStore::empty().permissive_legacy,
            "TrustStore::empty() must enforce the pubkey + chain checks"
        );
        // An empty store is not "permissive" — it is maximally strict: with
        // no anchors, `verify_chain` bottoms out in NoTrustAnchor.
        assert_eq!(TrustStore::empty().anchor_count(), 0);
        // And the test-only escape hatch really does flip it, so the two
        // assertions above are not vacuous.
        assert!(TrustStore::permissive_legacy_tests().permissive_legacy);
    }

    fn enc_len(len: usize) -> Vec<u8> {
        if len < 0x80 {
            vec![len as u8]
        } else if len < 0x100 {
            vec![0x81, len as u8]
        } else if len < 0x10000 {
            vec![0x82, (len >> 8) as u8, len as u8]
        } else {
            vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
        }
    }

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend(enc_len(content.len()));
        out.extend_from_slice(content);
        out
    }

    fn seq(content: &[u8]) -> Vec<u8> {
        tlv(TAG_SEQUENCE, content)
    }

    fn set(content: &[u8]) -> Vec<u8> {
        tlv(TAG_SET, content)
    }

    fn octet(content: &[u8]) -> Vec<u8> {
        tlv(TAG_OCTET_STRING, content)
    }

    fn integer(value: u64) -> Vec<u8> {
        if value == 0 {
            return tlv(TAG_INTEGER, &[0]);
        }
        let mut bytes = Vec::new();
        let mut v = value;
        while v > 0 {
            bytes.push((v & 0xFF) as u8);
            v >>= 8;
        }
        bytes.reverse();
        if bytes[0] & 0x80 != 0 {
            let mut content = vec![0u8];
            content.extend(bytes);
            return tlv(TAG_INTEGER, &content);
        }
        tlv(TAG_INTEGER, &bytes)
    }

    fn oid(dotted: &str) -> Vec<u8> {
        let arcs: Vec<u64> = dotted.split('.').map(|s| s.parse().unwrap()).collect();
        let mut content = vec![(arcs[0] * 40 + arcs[1]) as u8];
        for &a in &arcs[2..] {
            if a == 0 {
                content.push(0);
                continue;
            }
            let mut buf: Vec<u8> = Vec::new();
            let mut v = a;
            while v > 0 {
                buf.push((v & 0x7F) as u8);
                v >>= 7;
            }
            for (i, b) in buf.iter().rev().enumerate() {
                let last = i + 1 == buf.len();
                content.push(if last { *b } else { *b | 0x80 });
            }
        }
        tlv(TAG_OID, &content)
    }

    fn ctx_imp(n: u8, content: &[u8]) -> Vec<u8> {
        // Implicit context-specific constructed tag.
        tlv(0xA0 | (n & 0x1F), content)
    }

    fn algorithm_identifier(o: &str) -> Vec<u8> {
        let mut inner = oid(o);
        inner.extend_from_slice(&[TAG_NULL, 0x00]); // params NULL
        seq(&inner)
    }

    /// Build a minimal X.509-shaped SEQUENCE with a recognizable CN.
    /// The bytes are NOT a valid signed certificate — they only need to
    /// be a well-formed top-level SEQUENCE that our cert-set splitter
    /// keeps.  We embed a "marker" payload so the test can confirm the
    /// returned DER matches the input cert byte-for-byte.
    fn fake_cert_der(marker: &str) -> Vec<u8> {
        let inner = octet(marker.as_bytes());
        seq(&inner)
    }

    /// Build a valid IssuerAndSerialNumber containing a CN-only Name.
    fn issuer_and_serial(cn: &str, serial: u64) -> Vec<u8> {
        // Name ::= SEQUENCE OF RDN; each RDN is SET OF ATV.
        // ATV ::= SEQUENCE { type OID, value ANY (we use PrintableString=0x13) }
        let atv = seq(&[
            oid("2.5.4.3").as_slice(),
            tlv(0x13, cn.as_bytes()).as_slice(),
        ]
        .concat());
        let rdn = set(&atv);
        let name = seq(&rdn);
        let iasn_inner = [name.as_slice(), integer(serial).as_slice()].concat();
        seq(&iasn_inner)
    }

    /// Build a SignedData ContentInfo binding a single SignerInfo whose
    /// `messageDigest` authenticated attribute is `digest_alg(sf_bytes)`.
    /// `tamper_digest=true` replaces the computed digest with random
    /// bytes to exercise the integrity-rejection path.
    fn build_signer_block(sf_bytes: &[u8], digest_alg: DigestAlg, tamper_digest: bool) -> Vec<u8> {
        let dig = match digest_alg {
            DigestAlg::Sha256 => sha256::digest(sf_bytes).to_vec(),
            DigestAlg::Sha1 => sha1::digest(sf_bytes).to_vec(),
            _ => vec![0; 32],
        };
        let stored = if tamper_digest {
            vec![
                0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD,
                0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF,
                0xDE, 0xAD, 0xBE, 0xEF,
            ]
        } else {
            dig
        };
        let digest_oid_dotted = match digest_alg {
            DigestAlg::Sha256 => OID_SHA256,
            DigestAlg::Sha1 => OID_SHA1,
            _ => OID_SHA256,
        };

        // authenticatedAttributes [0] IMPLICIT SET OF Attribute
        let attr_content_type = seq(&[
            oid(OID_CONTENT_TYPE).as_slice(),
            set(&oid(OID_DATA)).as_slice(),
        ]
        .concat());
        let attr_message_digest = seq(&[
            oid(OID_MESSAGE_DIGEST).as_slice(),
            set(&octet(&stored)).as_slice(),
        ]
        .concat());
        let attrs_set_content =
            [attr_content_type.as_slice(), attr_message_digest.as_slice()].concat();
        let auth_attrs = ctx_imp(0, &attrs_set_content);

        // SignerInfo
        let signer_info = seq(&[
            integer(1).as_slice(),
            issuer_and_serial("TestSigner", 0x42).as_slice(),
            algorithm_identifier(digest_oid_dotted).as_slice(),
            auth_attrs.as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.1").as_slice(), // signatureAlgorithm
            octet(b"fake-signature-bytes").as_slice(),               // signatureValue
        ]
        .concat());

        // Certificates [0] IMPLICIT — one fake cert.
        let cert = fake_cert_der("MARKER-CERT");
        let certs = ctx_imp(0, &cert);

        // encapContentInfo ::= SEQUENCE { contentType OID }  (detached, no content)
        let encap = seq(&oid(OID_DATA));

        // digestAlgorithms SET
        let digest_algs = set(&algorithm_identifier(digest_oid_dotted));

        // SignedData SEQUENCE
        let signed_data = seq(&[
            integer(1).as_slice(),
            digest_algs.as_slice(),
            encap.as_slice(),
            certs.as_slice(),
            set(&signer_info).as_slice(),
        ]
        .concat());

        // Outer ContentInfo
        seq(&[
            oid(OID_SIGNED_DATA).as_slice(),
            ctx_imp(0, &signed_data).as_slice(),
        ]
        .concat())
    }

    #[test]
    fn verifies_self_consistent_sha256_signer_block() {
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: abc123=\r\n\r\n";
        let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ false);
        let vs =
            verify_signer_block(&block, sf, &legacy_ts()).expect("well-formed block should verify");
        assert_eq!(vs.digest_alg, DigestAlg::Sha256);
        assert_eq!(vs.chain.len(), 1);
        // Cert bytes must be the *embedded* cert DER, not the outer block.
        assert_eq!(vs.chain[0], fake_cert_der("MARKER-CERT"));
        // Principal extraction is best-effort but should contain the CN.
        assert!(
            vs.principal.contains("TestSigner"),
            "principal {:?} should contain CN value",
            vs.principal
        );
    }

    #[test]
    fn verifies_self_consistent_sha1_signer_block() {
        // Legacy jarsigner JARs (pre-JDK 9) used SHA-1.  We must still
        // accept those — but only when the .SF digest agrees.
        let sf = b"Signature-Version: 1.0\r\nSHA1-Digest-Manifest: xyz=\r\n\r\n";
        let block = build_signer_block(sf, DigestAlg::Sha1, /*tamper=*/ false);
        let vs = verify_signer_block(&block, sf, &legacy_ts()).expect("SHA-1 block should verify");
        assert_eq!(vs.digest_alg, DigestAlg::Sha1);
        assert_eq!(vs.chain.len(), 1);
    }

    #[test]
    fn rejects_tampered_sf_via_digest_mismatch() {
        // An attacker modifies the .SF after signing.  The messageDigest
        // attribute stored in the SignerInfo (which we cannot rewrite
        // without the signing key) will no longer equal SHA-256(.SF).
        let sf_original = b"Signature-Version: 1.0\r\n\r\n";
        let block = build_signer_block(sf_original, DigestAlg::Sha256, /*tamper=*/ false);

        let sf_tampered = b"Signature-Version: 1.0\r\nEvil: yes\r\n\r\n";
        assert!(
            verify_signer_block(&block, sf_tampered, &legacy_ts()).is_none(),
            "tampered .SF must be rejected"
        );

        // Sanity: original .SF still verifies.
        assert!(verify_signer_block(&block, sf_original, &legacy_ts()).is_some());
    }

    #[test]
    fn rejects_tampered_message_digest_attribute() {
        // The block itself has been tampered to flip the messageDigest
        // attribute (without recomputing the signature).  Even though
        // the .SF is genuine, the recomputed SHA-256 won't equal the
        // stored bytes.
        let sf = b"Signature-Version: 1.0\r\n\r\n";
        let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ true);
        assert!(verify_signer_block(&block, sf, &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_empty_block_without_panic() {
        assert!(verify_signer_block(&[], b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_truncated_block_without_panic() {
        // A 3-byte stub — not even enough for a TLV header / length.
        assert!(verify_signer_block(&[0x30, 0x82, 0xFF], b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_garbage_bytes_without_panic() {
        // 256 random-ish bytes that aren't valid DER.
        let garbage: Vec<u8> = (0..=255u8).collect();
        assert!(verify_signer_block(&garbage, b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_oversized_block_without_panic() {
        let huge = vec![0u8; MAX_SIGNER_BLOCK + 1];
        assert!(verify_signer_block(&huge, b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_block_with_wrong_outer_oid() {
        // Replace pkcs7-signedData with pkcs7-data — should bounce.
        let inner = seq(&oid("1.2.3.4.5"));
        let bad = seq(&[oid(OID_DATA).as_slice(), ctx_imp(0, &inner).as_slice()].concat());
        assert!(verify_signer_block(&bad, b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_block_missing_signer_info() {
        // SignedData with empty signerInfos SET.
        let signed_data = seq(&[
            integer(1).as_slice(),
            set(&algorithm_identifier(OID_SHA256)).as_slice(),
            seq(&oid(OID_DATA)).as_slice(), // encap
            set(&[]).as_slice(),            // empty signerInfos
        ]
        .concat());
        let block = seq(&[
            oid(OID_SIGNED_DATA).as_slice(),
            ctx_imp(0, &signed_data).as_slice(),
        ]
        .concat());
        assert!(verify_signer_block(&block, b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_block_missing_authenticated_attributes() {
        // The integrity check is anchored in the messageDigest
        // attribute; a SignerInfo without authenticatedAttributes must
        // be refused outright (otherwise an attacker just strips them).
        let signer_info = seq(&[
            integer(1).as_slice(),
            issuer_and_serial("TestSigner", 1).as_slice(),
            algorithm_identifier(OID_SHA256).as_slice(),
            // no [0] IMPLICIT attrs
            algorithm_identifier("1.2.840.113549.1.1.1").as_slice(),
            octet(b"sig").as_slice(),
        ]
        .concat());
        let cert = fake_cert_der("CERT");
        let signed_data = seq(&[
            integer(1).as_slice(),
            set(&algorithm_identifier(OID_SHA256)).as_slice(),
            seq(&oid(OID_DATA)).as_slice(),
            ctx_imp(0, &cert).as_slice(),
            set(&signer_info).as_slice(),
        ]
        .concat());
        let block = seq(&[
            oid(OID_SIGNED_DATA).as_slice(),
            ctx_imp(0, &signed_data).as_slice(),
        ]
        .concat());
        assert!(verify_signer_block(&block, b"sf", &legacy_ts()).is_none());
    }

    #[test]
    fn rejects_unsupported_digest_alg() {
        // SHA-512 — recognised OID but the implementation returns an
        // empty digest, which can never equal the stored 64-byte digest.
        let sf = b"any";
        let block = build_signer_block(sf, DigestAlg::Sha512, /*tamper=*/ false);
        assert!(verify_signer_block(&block, sf, &legacy_ts()).is_none());
    }

    #[test]
    fn decode_oid_handles_signed_data_oid() {
        let bytes = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
        assert_eq!(decode_oid(&bytes).unwrap(), OID_SIGNED_DATA);
    }

    #[test]
    fn decode_oid_rejects_truncated_arc() {
        // Continuation bit set on the last byte → truncated arc.
        let bytes = [0x2A, 0x86];
        assert!(decode_oid(&bytes).is_err());
    }

    // ---------------------------------------------------------------------
    // Task #40 — trust-store + chain verification tests
    //
    // Each fixture builds a synthetic X.509-shaped certificate whose
    // `signatureAlgorithm` is `OID_STUB_SIG` and whose `signatureValue`
    // is `SHA-256(TBSCertificate || parent.subject_dn)`.  The
    // synthetic algorithm is recognised exclusively by these tests —
    // production chains hitting `OID_STUB_SIG` would already be
    // rejected upstream as a non-standard OID.
    // ---------------------------------------------------------------------

    /// Build a `Name ::= SEQUENCE { RDN }` with a single CN attribute.
    fn x509_name(cn: &str) -> Vec<u8> {
        let atv = seq(&[
            oid("2.5.4.3").as_slice(),
            tlv(0x13, cn.as_bytes()).as_slice(),
        ]
        .concat());
        let rdn = set(&atv);
        seq(&rdn)
    }

    /// Minimal Validity ::= SEQUENCE { UTCTime, UTCTime }.  Values are
    /// fixed strings — the chain code doesn't look at them.
    fn x509_validity() -> Vec<u8> {
        let nb = tlv(0x17, b"260101000000Z"); // 2026-01-01
        let na = tlv(0x17, b"360101000000Z"); // 2036-01-01
        seq(&[nb.as_slice(), na.as_slice()].concat())
    }

    /// Minimal SubjectPublicKeyInfo ::= SEQUENCE { AlgorithmIdentifier,
    /// BIT STRING }.  We pin the subject DN of the cert as the
    /// "public key" bytes so that [`X509Cert::link_signature_ok`] can
    /// recover the parent's identity from the child's signature
    /// equation.  This is a stand-in for real RSA/ECDSA encoding.
    fn x509_spki(subject_dn: &[u8]) -> Vec<u8> {
        let algid = algorithm_identifier(OID_STUB_SIG);
        // BIT STRING = leading 0x00 (unused bits) || subject_dn
        let mut bs = vec![0u8];
        bs.extend_from_slice(subject_dn);
        let bit_string = tlv(0x03, &bs);
        seq(&[algid.as_slice(), bit_string.as_slice()].concat())
    }

    /// Build the TBSCertificate body for a synthetic X.509 cert.
    fn build_tbs(subject_cn: &str, issuer_cn: &str) -> (Vec<u8>, Vec<u8>) {
        let subject_dn = x509_name(subject_cn);
        let issuer_dn = x509_name(issuer_cn);
        let version = ctx_imp(0, &integer(2)); // v3
        let serial = integer(1);
        let signature_alg = algorithm_identifier(OID_STUB_SIG);
        let validity = x509_validity();
        let spki = x509_spki(&subject_dn);
        let tbs_body = [
            version.as_slice(),
            serial.as_slice(),
            signature_alg.as_slice(),
            issuer_dn.as_slice(),
            validity.as_slice(),
            subject_dn.as_slice(),
            spki.as_slice(),
        ]
        .concat();
        (seq(&tbs_body), subject_dn)
    }

    /// Build a complete X.509 certificate DER, signing with the
    /// synthetic stub algorithm.  `parent_subject_dn` is the DER of the
    /// parent's `subject` Name TLV — for a root, pass its own
    /// subject_dn (self-signed).
    fn build_x509_cert(subject_cn: &str, issuer_cn: &str, parent_subject_dn: &[u8]) -> Vec<u8> {
        let (tbs, _) = build_tbs(subject_cn, issuer_cn);
        // signature value = SHA-256(tbs || parent_subject_dn), as a
        // BIT STRING with 0 unused bits.
        let mut buf = Vec::with_capacity(tbs.len() + parent_subject_dn.len());
        buf.extend_from_slice(&tbs);
        buf.extend_from_slice(parent_subject_dn);
        let sig = sha256::digest(&buf);
        let mut bs = vec![0u8];
        bs.extend_from_slice(&sig);
        let bit_string = tlv(0x03, &bs);
        let sig_alg = algorithm_identifier(OID_STUB_SIG);
        seq(&[tbs.as_slice(), sig_alg.as_slice(), bit_string.as_slice()].concat())
    }

    #[test]
    fn task40_self_signed_chain_rejected_without_anchor() {
        // Acceptance #4 case (a): self-signed leaf with no trust anchor
        // present.  verify_chain must refuse with NoTrustAnchor — the
        // exact attack the original task description called out.
        let root_subject_dn = x509_name("SelfSigner");
        let cert_der = build_x509_cert("SelfSigner", "SelfSigner", &root_subject_dn);
        let leaf = X509Cert::parse(&cert_der).expect("parse self-signed");
        assert!(leaf.is_self_signed(), "fixture must be self-signed");

        let ts = TrustStore::empty();
        let err = verify_chain(&leaf, &[], &ts).expect_err("must reject");
        assert_eq!(err, TrustError::NoTrustAnchor);
    }

    #[test]
    fn task40_chain_rooted_in_test_anchor_verifies() {
        // Acceptance #4 case (b): leaf → intermediate → root anchor.
        // Trust store contains the root; verify_chain must succeed.
        let root_subject_dn = x509_name("RootCA");
        let root_der = build_x509_cert("RootCA", "RootCA", &root_subject_dn);

        let int_subject_dn = x509_name("IntermediateCA");
        // Intermediate is signed by the root → parent_subject_dn = root_subject_dn.
        let int_der = build_x509_cert("IntermediateCA", "RootCA", &root_subject_dn);

        // Leaf is signed by the intermediate → parent_subject_dn = int_subject_dn.
        let leaf_der = build_x509_cert("LeafSigner", "IntermediateCA", &int_subject_dn);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let int_cert = X509Cert::parse(&int_der).expect("parse intermediate");

        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root_der.clone()));
        assert_eq!(ts.anchor_count(), 1);

        verify_chain(&leaf, &[int_cert], &ts).expect("must verify against in-store root");
    }

    #[test]
    fn task40_chain_with_broken_intermediate_signature_fails() {
        // Acceptance #4 case (c): chain whose **intermediate** cert
        // has been tampered after signing — the link from the
        // intermediate up to the root anchor must reject because the
        // intermediate's signatureValue no longer covers its TBS.
        let root_subject_dn = x509_name("RootCA");
        let root_der = build_x509_cert("RootCA", "RootCA", &root_subject_dn);

        let int_subject_dn = x509_name("IntermediateCA");
        // Mint a legitimately-signed intermediate, then tamper its
        // signature byte to break the intermediate→root link
        // specifically.  The leaf→intermediate link must STILL be
        // valid so the walk reaches the broken step.
        let mut int_der = build_x509_cert("IntermediateCA", "RootCA", &root_subject_dn);
        let last = int_der.len() - 1;
        int_der[last] ^= 0xFF;

        let leaf_der = build_x509_cert("LeafSigner", "IntermediateCA", &int_subject_dn);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let int_cert = X509Cert::parse(&int_der).expect("parse intermediate");
        let mut ts = TrustStore::empty();
        ts.add_anchor_der(root_der);

        let err = verify_chain(&leaf, &[int_cert], &ts).expect_err("must reject");
        assert_eq!(err, TrustError::BadSignature);
    }

    #[test]
    fn task40_trust_store_load_default_records_sources() {
        // load_default should always at minimum push the placeholder
        // entries for the system-root-store seam and (optionally) the
        // JDK cacerts path.  We just check the type contract.
        let ts = TrustStore::load_default();
        assert!(
            !ts.sources_loaded.is_empty(),
            "load_default must record at least the system-root placeholder line"
        );
        assert!(
            ts.sources_loaded
                .iter()
                .any(|s| s.starts_with("system-root-store")),
            "system-root-store source line missing from {:?}",
            ts.sources_loaded
        );
    }

    #[test]
    fn task40_real_crypto_algorithm_returns_not_implemented() {
        // A chain link whose outer signatureAlgorithm names a real OID
        // (here sha256WithRSAEncryption, 1.2.840.113549.1.1.11) but whose
        // *parent* carries a synthetic stub SPKI (the test fixtures use
        // `OID_STUB_SIG` as the key-algorithm OID, not a real RSA/EC/DSA
        // key) must be rejected as NotImplemented.  `parse_spki` maps the
        // unknown key OID to `PublicKey::Other`, which cannot bind to the
        // RSA verifier → `SigVerify::Unsupported` → `TrustError::
        // NotImplemented` (fail-closed).  NOTE: RSA/ECDSA/DSA verification
        // is fully implemented — a link with a *real* key of the matching
        // family and a bad signature returns `BadSignature` instead; this
        // case only exercises the unknown-key-type fail-closed path.
        let root_subject_dn = x509_name("RealRoot");
        // Hand-roll a cert whose outer signatureAlgorithm OID is the
        // real PKCS#1 v1.5 RSA-SHA256 identifier.
        let (tbs, _) = build_tbs("RealLeaf", "RealRoot");
        let sig_alg = algorithm_identifier("1.2.840.113549.1.1.11");
        let bit_string = tlv(0x03, &[0u8, 0xDE, 0xAD]); // bogus sig bytes
        let leaf_der = seq(&[tbs.as_slice(), sig_alg.as_slice(), bit_string.as_slice()].concat());

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let mut ts = TrustStore::empty();
        // Make a (mismatched) self-signed root anchor with stub-sig so
        // the DN lookup succeeds and we drive the signature step.
        let root_der = build_x509_cert("RealRoot", "RealRoot", &root_subject_dn);
        ts.add_anchor_der(root_der);
        let err = verify_chain(&leaf, &[], &ts).expect_err("real RSA not implemented");
        assert_eq!(err, TrustError::NotImplemented);
    }

    #[test]
    fn task40_pem_bundle_decode_round_trip() {
        // PEM-encode a synthetic anchor DER and confirm
        // TrustStore::load_pem_bundle picks it up.  This exercises the
        // local base64 decoder.
        let root_subject_dn = x509_name("PemRoot");
        let root_der = build_x509_cert("PemRoot", "PemRoot", &root_subject_dn);

        // Hand-roll base64 (no `base64` dep) using std::fmt.
        fn b64(input: &[u8]) -> String {
            const ALPHA: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
            let mut i = 0;
            while i + 3 <= input.len() {
                let b0 = input[i] as u32;
                let b1 = input[i + 1] as u32;
                let b2 = input[i + 2] as u32;
                let n = (b0 << 16) | (b1 << 8) | b2;
                out.push(ALPHA[((n >> 18) & 63) as usize] as char);
                out.push(ALPHA[((n >> 12) & 63) as usize] as char);
                out.push(ALPHA[((n >> 6) & 63) as usize] as char);
                out.push(ALPHA[(n & 63) as usize] as char);
                i += 3;
            }
            let rem = input.len() - i;
            if rem == 1 {
                let n = (input[i] as u32) << 16;
                out.push(ALPHA[((n >> 18) & 63) as usize] as char);
                out.push(ALPHA[((n >> 12) & 63) as usize] as char);
                out.push('=');
                out.push('=');
            } else if rem == 2 {
                let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
                out.push(ALPHA[((n >> 18) & 63) as usize] as char);
                out.push(ALPHA[((n >> 12) & 63) as usize] as char);
                out.push(ALPHA[((n >> 6) & 63) as usize] as char);
                out.push('=');
            }
            out
        }

        let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
        for chunk in b64(&root_der).as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");

        let mut ts = TrustStore::empty();
        let n = ts.load_pem_bundle(&pem);
        assert_eq!(n, 1, "exactly one anchor must be decoded");
        assert_eq!(ts.anchor_count(), 1);
    }

    // ---------------------------------------------------------------------
    // Task #1 — real RSA PKCS#1 v1.5 verification tests.
    //
    // We use a fixed, small (512-bit) textbook RSA key.  512-bit RSA is
    // cryptographically dead, but it exercises the *exact* code path real
    // 2048/4096-bit jarsigner keys use (modpow + DigestInfo compare) at a
    // size the in-test signer can produce quickly.  Signing in the test is
    // `DigestInfo^d mod n`; verification is the production `sig^e mod n`.
    // ---------------------------------------------------------------------

    // A real 512-bit RSA key (n = p*q, e = 65537, d = e^-1 mod phi(n)).
    // Generated offline; embedded big-endian.  512-bit is dead crypto but
    // exercises the exact production code path (modpow + DigestInfo).
    const RSA512_N: &[u8] = &[
        0x9f, 0x99, 0x9b, 0x45, 0xc9, 0xaf, 0xc0, 0x21, 0xf5, 0x6b, 0x81, 0xb7, 0x57, 0xf2, 0x4c,
        0x82, 0x8e, 0x8a, 0xca, 0xa8, 0xb9, 0xc6, 0xbd, 0x61, 0xab, 0xd5, 0xe2, 0xea, 0x35, 0x8c,
        0xd0, 0x63, 0x4b, 0xee, 0x4c, 0x67, 0x39, 0x79, 0x70, 0x41, 0x72, 0xd8, 0xcb, 0xaa, 0x7d,
        0x6d, 0x4e, 0x29, 0xa2, 0x52, 0xa0, 0x26, 0x7a, 0xd9, 0x55, 0x8c, 0xdb, 0xce, 0x22, 0xf2,
        0xc1, 0xc5, 0x9c, 0x09,
    ];
    const RSA512_D: &[u8] = &[
        0x3e, 0x1c, 0x88, 0x8a, 0x1b, 0x58, 0xb3, 0x7c, 0x43, 0xc7, 0xa7, 0xfe, 0xd3, 0x52, 0x2f,
        0xa6, 0x6b, 0x94, 0xe6, 0x13, 0xcd, 0xe0, 0xe3, 0x58, 0xfc, 0x87, 0xcb, 0xbc, 0x7c, 0x44,
        0xa5, 0xe0, 0x2e, 0xc1, 0x4e, 0xbf, 0xc0, 0x4f, 0xc8, 0x72, 0x25, 0x58, 0xff, 0x5c, 0x85,
        0x9b, 0x88, 0x23, 0xa1, 0x79, 0x3a, 0x53, 0xeb, 0x3d, 0x82, 0xc5, 0x94, 0x80, 0x47, 0x9a,
        0x39, 0x1d, 0x6c, 0x01,
    ];

    // A 1024-bit RSA test key (k = 128).  The 512-bit key above cannot hold a
    // PKCS#1 v1.5 EMSA block for SHA-384/512 (a 64-byte hash + 19-byte
    // DigestInfo prefix + 11-byte minimum padding needs k >= 94), so the
    // round-trip test for those digests requires a larger modulus.
    const RSA1024_N: &[u8] = &[
        0xe5, 0x2f, 0x42, 0xb1, 0xda, 0x1c, 0x87, 0xb6, 0xcf, 0x39, 0x39, 0xd8, 0x78, 0xc2, 0x3b,
        0x59, 0x53, 0x5b, 0x0f, 0x3a, 0xd8, 0x47, 0xb2, 0x07, 0xd7, 0xb2, 0xdc, 0x49, 0x97, 0xbc,
        0x32, 0xbe, 0x5e, 0x6f, 0xfa, 0xd7, 0xbd, 0xf0, 0x83, 0xb1, 0xfe, 0xe0, 0xc6, 0xaf, 0x69,
        0x84, 0xec, 0x36, 0x90, 0xb6, 0x9f, 0xc3, 0x0a, 0xd8, 0x8b, 0x53, 0x15, 0x4b, 0x1c, 0xaa,
        0xd7, 0x02, 0xf6, 0x0c, 0x94, 0x6f, 0xc8, 0x65, 0xec, 0x8c, 0xf6, 0xb0, 0x6f, 0x03, 0x53,
        0xcc, 0x69, 0x28, 0x41, 0x47, 0x6e, 0x9d, 0x3b, 0x85, 0x3e, 0xd7, 0xed, 0xd5, 0xf7, 0x23,
        0xa8, 0x4b, 0xbc, 0xa7, 0x2d, 0xaa, 0xa3, 0x6b, 0xac, 0xfa, 0xe6, 0x69, 0xbd, 0x13, 0x1c,
        0xcc, 0x3b, 0x08, 0x27, 0x3c, 0x71, 0x2c, 0x83, 0xf0, 0x07, 0x94, 0xfc, 0x0d, 0x15, 0x01,
        0x5a, 0x98, 0x65, 0x34, 0x46, 0x16, 0x20, 0x45,
    ];
    const RSA1024_D: &[u8] = &[
        0x9d, 0xa6, 0x41, 0xd1, 0x87, 0x80, 0x62, 0x96, 0x8c, 0xbb, 0x07, 0xa0, 0x71, 0x88, 0xe2,
        0x3c, 0x52, 0xcb, 0x6b, 0x91, 0x85, 0xde, 0xe3, 0x86, 0xe3, 0x88, 0x24, 0x61, 0xf7, 0x1f,
        0x3d, 0x24, 0x98, 0x5f, 0x9d, 0x04, 0x34, 0xa2, 0xb2, 0x64, 0x89, 0x37, 0xe3, 0x54, 0x1c,
        0x58, 0x94, 0x08, 0x00, 0xc9, 0xae, 0xe2, 0x02, 0x9e, 0xec, 0x4f, 0xcd, 0x70, 0xea, 0x9a,
        0x55, 0xe6, 0xb2, 0x8a, 0xad, 0x7c, 0x0f, 0xd5, 0x27, 0x70, 0x73, 0x72, 0x31, 0x1b, 0x75,
        0xc6, 0x21, 0x0e, 0x8b, 0x9d, 0x88, 0x99, 0x1a, 0xe6, 0xcd, 0xc0, 0x8c, 0x71, 0x63, 0xa4,
        0xa6, 0x6a, 0x0c, 0x95, 0x85, 0xc0, 0x35, 0xda, 0x5f, 0xc5, 0xc7, 0x65, 0x24, 0x42, 0xb7,
        0xf3, 0x1e, 0xb6, 0xbe, 0x95, 0x03, 0x00, 0xea, 0x09, 0x0a, 0xc3, 0xd8, 0x52, 0xa0, 0x9f,
        0x0d, 0x12, 0xb9, 0xbf, 0x3f, 0xf1, 0x72, 0xad,
    ];

    /// Build an RSA `SubjectPublicKeyInfo` from raw big-endian n + e.
    fn rsa_spki(n_be: &[u8], e: u64) -> Vec<u8> {
        // INTEGER encoding: prepend 0x00 if the high bit is set (positive).
        fn der_int(bytes: &[u8]) -> Vec<u8> {
            let mut b = bytes.to_vec();
            // Strip leading zero bytes except one that guards a high bit.
            while b.len() > 1 && b[0] == 0 && b[1] & 0x80 == 0 {
                b.remove(0);
            }
            if b[0] & 0x80 != 0 {
                let mut g = vec![0u8];
                g.extend_from_slice(&b);
                b = g;
            }
            tlv(TAG_INTEGER, &b)
        }
        let mut e_be = Vec::new();
        let mut v = e;
        while v > 0 {
            e_be.insert(0, (v & 0xFF) as u8);
            v >>= 8;
        }
        let rsa_pub = seq(&[der_int(n_be).as_slice(), der_int(&e_be).as_slice()].concat());
        let algid = {
            let mut inner = oid("1.2.840.113549.1.1.1");
            inner.extend_from_slice(&[TAG_NULL, 0x00]);
            seq(&inner)
        };
        let mut bs = vec![0u8];
        bs.extend_from_slice(&rsa_pub);
        let bit_string = tlv(0x03, &bs);
        seq(&[algid.as_slice(), bit_string.as_slice()].concat())
    }

    #[test]
    fn sha384_512_digests_match_known_vectors() {
        // NIST FIPS 180-4 example: SHA-384/512 of "abc".
        let abc = b"abc";
        let d384 = super::sha2ext::sha384(abc);
        let d512 = super::sha2ext::sha512(abc);
        // First bytes of the well-known digests.
        assert_eq!(&d384[..4], &[0xcb, 0x00, 0x75, 0x3f]);
        assert_eq!(&d512[..4], &[0xdd, 0xaf, 0x35, 0xa1]);
    }

    #[test]
    fn parse_spki_recovers_rsa_key() {
        let spki = rsa_spki(RSA512_N, 65537);
        match super::parse_spki(&spki).expect("parse rsa spki") {
            super::PublicKey::Rsa(k) => {
                assert_eq!(k.k, 64, "512-bit modulus => k = 64 bytes");
                assert_eq!(
                    k.e.cmp(&super::BigUint::from_bytes_be(&[1, 0, 1])),
                    std::cmp::Ordering::Equal
                );
            }
            _ => panic!("expected RSA key"),
        }
    }

    /// Produce an RSA PKCS#1 v1.5 signature for `message` under the test
    /// key, using the production `BigUint::modpow` with the private `d`.
    fn rsa_sign(message: &[u8], digest_alg: DigestAlg, n_be: &[u8], d_be: &[u8]) -> Vec<u8> {
        use super::BigUint;
        let n = BigUint::from_bytes_be(n_be);
        let d = BigUint::from_bytes_be(d_be);
        let k = (n.bit_length() + 7) / 8;
        let hash = super::raw_digest(digest_alg, message);
        let prefix = super::digest_info_prefix(digest_alg);
        let t_len = prefix.len() + hash.len();
        let ps_len = k - t_len - 3;
        let mut em = Vec::with_capacity(k);
        em.push(0x00);
        em.push(0x01);
        em.extend(std::iter::repeat(0xff).take(ps_len));
        em.push(0x00);
        em.extend_from_slice(prefix);
        em.extend_from_slice(&hash);
        let m = BigUint::from_bytes_be(&em);
        m.modpow(&d, &n).to_bytes_be_padded(k)
    }

    #[test]
    fn rsa_verify_accepts_valid_signature() {
        use super::{DigestAlg, SigVerify};
        // Use the 1024-bit key so even SHA-512's EMSA block fits the modulus.
        let spki = rsa_spki(RSA1024_N, 65537);
        for alg in [
            DigestAlg::Sha1,
            DigestAlg::Sha256,
            DigestAlg::Sha384,
            DigestAlg::Sha512,
        ] {
            let msg = b"the quick brown fox";
            let sig = rsa_sign(msg, alg, RSA1024_N, RSA1024_D);
            assert_eq!(
                verify_signature_with_spki(&spki, sig_alg_oid_for(alg), msg, &sig),
                SigVerify::Ok,
                "valid {:?} signature must verify",
                alg
            );
            // Tampered message → Bad.
            assert_eq!(
                verify_signature_with_spki(&spki, sig_alg_oid_for(alg), b"tampered", &sig),
                SigVerify::Bad
            );
            // Tampered signature → Bad.
            let mut bad = sig.clone();
            bad[10] ^= 0xFF;
            assert_eq!(
                verify_signature_with_spki(&spki, sig_alg_oid_for(alg), msg, &bad),
                SigVerify::Bad
            );
        }
    }

    fn sig_alg_oid_for(alg: DigestAlg) -> &'static str {
        match alg {
            DigestAlg::Sha1 => "1.2.840.113549.1.1.5",
            DigestAlg::Sha256 => "1.2.840.113549.1.1.11",
            DigestAlg::Sha384 => "1.2.840.113549.1.1.12",
            DigestAlg::Sha512 => "1.2.840.113549.1.1.13",
        }
    }

    #[test]
    fn rsa_verify_rejects_structural_garbage() {
        use super::{DigestAlg, SigVerify};
        let spki = rsa_spki(RSA512_N, 65537);
        let key = match super::parse_spki(&spki).unwrap() {
            super::PublicKey::Rsa(k) => k,
            _ => unreachable!(),
        };
        // Wrong signature length → `Unsupported`, NOT `Bad`.
        //
        // SunRsaSign raises `SignatureException("Signature length not
        // correct")` here, *before* any RSA operation runs — so no
        // verification decision was reached. Reporting it as `Bad` would
        // claim we compared this signature against the key and found it
        // wanting, which we did not. Both outcomes refuse; only one is
        // honest about why. (A genuine mismatch always has the correct
        // length, so this arm can never swallow a real negative — see the
        // `s == n` case below, which does stay `Bad`.)
        assert_eq!(
            super::rsa_pkcs1v15_verify(&key, DigestAlg::Sha256, b"msg", &[0u8; 10]),
            SigVerify::Unsupported,
            "a malformed signature encoding is 'not checked', not 'checked and failed'"
        );
        // s == n (>= n): correct length, so the RSA operation really does
        // run and really does reject → the preserved negative, `Bad`.
        let n_be = key.n.to_bytes_be_padded(key.k);
        assert_eq!(
            super::rsa_pkcs1v15_verify(&key, DigestAlg::Sha256, b"msg", &n_be),
            SigVerify::Bad
        );
    }

    #[test]
    fn end_to_end_real_rsa_signer_block_and_chain() {
        use super::*;
        // Build a real RSA-signed SignerInfo over a `.SF`, plus a leaf cert
        // whose SPKI is the RSA test key and whose chain roots in a trust
        // anchor.  This exercises BOTH the SignerInfo pubkey check and the
        // chain link RSA verify on the production path (no permissive mode).

        // --- Leaf cert: subject = "RsaLeaf", issuer = "RsaRoot", RSA SPKI,
        //     self-... no: signed by root.  We sign the leaf TBS with the
        //     same test key for simplicity (root SPKI == same key).
        let leaf_spki = rsa_spki(RSA512_N, 65537);
        let subject_dn = x509_name("RsaLeaf");
        let issuer_dn = x509_name("RsaRoot");
        let root_dn = x509_name("RsaRoot");
        // Validity window covering "now" (2020..2099).
        let validity = seq(&[
            tlv(0x17, b"200101000000Z").as_slice(),
            tlv(0x18, b"20990101000000Z").as_slice(),
        ]
        .concat());
        let build_cert = |subj: &[u8], iss: &[u8]| -> Vec<u8> {
            let tbs = seq(&[
                ctx_imp(0, &integer(2)).as_slice(),
                integer(1).as_slice(),
                algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
                iss,
                validity.as_slice(),
                subj,
                leaf_spki.as_slice(),
            ]
            .concat());
            // Sign the TBS with the RSA key (sha256WithRSA).
            let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
            let mut bs = vec![0u8];
            bs.extend_from_slice(&sig);
            seq(&[
                tbs.as_slice(),
                algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
                tlv(0x03, &bs).as_slice(),
            ]
            .concat())
        };
        let leaf_der = build_cert(&subject_dn, &issuer_dn);
        let root_der = build_cert(&root_dn, &root_dn); // self-signed root

        // --- SignerInfo / SignedData over a `.SF`.
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: zzz=\r\n\r\n";
        let dig = sha256::digest(sf).to_vec();
        let attr_ct = seq(&[
            oid(OID_CONTENT_TYPE).as_slice(),
            set(&oid(OID_DATA)).as_slice(),
        ]
        .concat());
        let attr_md = seq(&[
            oid(OID_MESSAGE_DIGEST).as_slice(),
            set(&octet(&dig)).as_slice(),
        ]
        .concat());
        let attrs_inner = [attr_ct.as_slice(), attr_md.as_slice()].concat();
        // SignedAttributes signed form = explicit SET.
        let signed_attrs_der = set(&attrs_inner);
        let si_sig = rsa_sign(&signed_attrs_der, DigestAlg::Sha256, RSA512_N, RSA512_D);
        let auth_attrs = ctx_imp(0, &attrs_inner);

        let signer_info = seq(&[
            integer(1).as_slice(),
            issuer_and_serial("RsaRoot", 1).as_slice(),
            algorithm_identifier(OID_SHA256).as_slice(),
            auth_attrs.as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            octet(&si_sig).as_slice(),
        ]
        .concat());

        // certificates [0] IMPLICIT: leaf first, then root.
        let mut certs_concat = leaf_der.clone();
        certs_concat.extend_from_slice(&root_der);
        let certs = ctx_imp(0, &certs_concat);
        let encap = seq(&oid(OID_DATA));
        let digest_algs = set(&algorithm_identifier(OID_SHA256));
        let signed_data = seq(&[
            integer(1).as_slice(),
            digest_algs.as_slice(),
            encap.as_slice(),
            certs.as_slice(),
            set(&signer_info).as_slice(),
        ]
        .concat());
        let block = seq(&[
            oid(OID_SIGNED_DATA).as_slice(),
            ctx_imp(0, &signed_data).as_slice(),
        ]
        .concat());

        // Trust store with the root anchor.
        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root_der.clone()));

        let vs = verify_signer_block(&block, sf, &ts)
            .expect("real RSA signer block + chain must verify end-to-end");
        assert_eq!(vs.digest_alg, DigestAlg::Sha256);
        assert_eq!(vs.chain.len(), 2);

        // Negative: tamper the .SF → reject.
        let bad_sf = b"Signature-Version: 1.0\r\nEvil: 1\r\n\r\n";
        assert!(verify_signer_block(&block, bad_sf, &ts).is_none());

        // Negative: empty trust store → no anchor → reject.
        assert!(verify_signer_block(&block, sf, &TrustStore::empty()).is_none());
    }

    #[test]
    fn ecdsa_malformed_ec_key_is_bad_not_accepted() {
        use super::{verify_signature_with_spki, SigVerify};
        // FEAT(jar-signer): an EC SPKI carrying an all-zero (off-curve)
        // "point" must NEVER verify.  The point fails to decode, so no
        // ECDSA verification runs at all → `Unsupported` (fail-closed,
        // never `Ok`; and deliberately not `Bad`, which would claim a
        // verdict we never reached).
        let ec_algid = {
            let mut inner = oid("1.2.840.10045.2.1"); // id-ecPublicKey
            inner.extend_from_slice(&oid("1.2.840.10045.3.1.7")); // prime256v1
            seq(&inner)
        };
        let bs = vec![0u8; 65]; // uncompressed point placeholder (off-curve)
        let mut bit = vec![0u8];
        bit.extend_from_slice(&bs);
        let ec_spki = seq(&[ec_algid.as_slice(), tlv(0x03, &bit).as_slice()].concat());
        let r = verify_signature_with_spki(
            &ec_spki,
            "1.2.840.10045.4.3.2", // ecdsa-with-SHA256
            b"message",
            &[0u8; 64],
        );
        assert_ne!(r, SigVerify::Ok, "off-curve EC key must never verify");
    }

    #[test]
    fn ecdsa_p256_round_trip_real_signature() {
        use super::{verify_signature_with_spki, DigestAlg, SigVerify};
        use p256::ecdsa::signature::hazmat::PrehashSigner;
        use p256::ecdsa::SigningKey;
        use spki::EncodePublicKey;

        // Fixed 32-byte scalar → deterministic key (no RNG needed).
        let sk = SigningKey::from_slice(&[0x11u8; 32]).expect("p256 signing key");
        let vk = sk.verifying_key();
        // `EncodePublicKey` on the elliptic-curve `PublicKey` is gated on
        // `pkcs8` (which we enable); the ecdsa `VerifyingKey` wrapper's
        // own impl is gated on `pem` (which we don't), so convert.
        let pk = p256::PublicKey::from(vk);
        let spki = pk
            .to_public_key_der()
            .expect("spki encode")
            .as_bytes()
            .to_vec();

        let msg = b"jarsigner-signed-attributes-bytes";
        // ECDSA-with-SHA256: sign the SHA-256 prehash; emit DER (r,s).
        let prehash = super::raw_digest(DigestAlg::Sha256, msg);
        let der_sig: ecdsa::der::Signature<p256::NistP256> =
            sk.sign_prehash(&prehash).expect("p256 sign");
        let sig_der = der_sig.as_bytes().to_vec();

        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", msg, &sig_der),
            SigVerify::Ok,
            "valid P-256 ECDSA signature must verify"
        );
        // Tampered message → Bad.
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", b"evil", &sig_der),
            SigVerify::Bad
        );
        // The SPKI must classify as EcP256.
        assert!(matches!(
            super::parse_spki(&spki).unwrap(),
            super::PublicKey::EcP256(_)
        ));
    }

    #[test]
    fn ecdsa_p384_round_trip_real_signature() {
        use super::{verify_signature_with_spki, DigestAlg, SigVerify};
        use p384::ecdsa::signature::hazmat::PrehashSigner;
        use p384::ecdsa::SigningKey;
        use spki::EncodePublicKey;

        let sk = SigningKey::from_slice(&[0x22u8; 48]).expect("p384 signing key");
        let vk = sk.verifying_key();
        let pk = p384::PublicKey::from(vk);
        let spki = pk
            .to_public_key_der()
            .expect("spki encode")
            .as_bytes()
            .to_vec();

        let msg = b"another signed-attrs blob";
        let prehash = super::raw_digest(DigestAlg::Sha384, msg);
        let der_sig: ecdsa::der::Signature<p384::NistP384> =
            sk.sign_prehash(&prehash).expect("p384 sign");
        let sig_der = der_sig.as_bytes().to_vec();

        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10045.4.3.3", msg, &sig_der),
            SigVerify::Ok,
            "valid P-384 ECDSA signature must verify"
        );
        assert!(matches!(
            super::parse_spki(&spki).unwrap(),
            super::PublicKey::EcP384(_)
        ));
    }

    /// Parse a run of hex into big-endian bytes (test helper for DSA params).
    fn hexbytes(h: &str) -> Vec<u8> {
        let cleaned: Vec<u8> = h.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
        cleaned
            .chunks(2)
            .map(|c| {
                let hi = (c[0] as char).to_digit(16).unwrap();
                let lo = (c[1] as char).to_digit(16).unwrap();
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    #[test]
    fn dsa_round_trip_real_signature() {
        use super::{verify_signature_with_spki, DigestAlg, SigVerify};
        use dsa::pkcs8::EncodePublicKey;
        use dsa::signature::hazmat::PrehashSigner;
        use dsa::{BigUint, Components, SigningKey, VerifyingKey};

        // RFC 6979 A.2.1 1024-bit DSA test key (same vector the `dsa`
        // crate ships in its own tests — exercises the real verify path).
        let p = BigUint::from_bytes_be(&hexbytes(
            "86F5CA03DCFEB225063FF830A0C769B9DD9D6153AD91D7CE27F787C43278B447\
             E6533B86B18BED6E8A48B784A14C252C5BE0DBF60B86D6385BD2F12FB763ED88\
             73ABFD3F5BA2E0A8C0A59082EAC056935E529DAF7C610467899C77ADEDFC846C\
             881870B7B19B2B58F9BE0521A17002E3BDD6B86685EE90B3D9A1B02B782B1779",
        ));
        let q = BigUint::from_bytes_be(&hexbytes("996F967F6C8E388D9E28D01E205FBA957A5698B1"));
        let g = BigUint::from_bytes_be(&hexbytes(
            "07B0F92546150B62514BB771E2A0C0CE387F03BDA6C56B505209FF25FD3C133D\
             89BBCD97E904E09114D9A7DEFDEADFC9078EA544D2E401AEECC40BB9FBBF78FD\
             87995A10A1C27CB7789B594BA7EFB5C4326A9FE59A070E136DB77175464ADCA4\
             17BE5DCE2F40D10A46A3A3943F26AB7FD9C0398FF8C76EE0A56826A8A88F1DBD",
        ));
        let x = BigUint::from_bytes_be(&hexbytes("411602CB19A6CCC34494D79D98EF1E7ED5AF25F7"));
        let y = BigUint::from_bytes_be(&hexbytes(
            "5DF5E01DED31D0297E274E1691C192FE5868FEF9E19A84776454B100CF16F653\
             92195A38B90523E2542EE61871C0440CB87C322FC4B4D2EC5E1E7EC766E1BE8D\
             4CE935437DC11C3C8FD426338933EBFE739CB3465F4D3668C5E473508253B1E6\
             82F65CBDC4FAE93C2EA212390E54905A86E2223170B44EAA7DA5DD9FFCFB7F3B",
        ));
        let components = Components::from_components(p, q, g).expect("dsa components");
        let vk = VerifyingKey::from_components(components, y).expect("dsa verifying key");
        let sk = SigningKey::from_components(vk.clone(), x).expect("dsa signing key");

        let spki = vk
            .to_public_key_der()
            .expect("dsa spki")
            .as_bytes()
            .to_vec();
        assert!(matches!(
            super::parse_spki(&spki).unwrap(),
            super::PublicKey::Dsa(_)
        ));

        let msg = b"dsa-signed .SF attributes";
        let prehash = super::raw_digest(DigestAlg::Sha1, msg);
        let sig: dsa::Signature = sk.sign_prehash(&prehash).expect("dsa sign");
        let sig_der = {
            use dsa::signature::SignatureEncoding;
            sig.to_bytes().to_vec()
        };

        // id-dsa-with-sha1 (1.2.840.10040.4.3).
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10040.4.3", msg, &sig_der),
            SigVerify::Ok,
            "valid DSA signature must verify"
        );
        // Tampered message → Bad.
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10040.4.3", b"evil", &sig_der),
            SigVerify::Bad
        );
    }

    // ---------------------------------------------------------------------
    // FEAT(jar-signer) — RFC 5280 extension processing tests.
    //
    // These build *real* RSA-signed certs (reusing the RSA-512 test key)
    // carrying X.509v3 extensions, then drive them through `verify_chain`
    // on the production path (no stub-sig short-circuit).
    // ---------------------------------------------------------------------

    /// Build a real RSA (sha256WithRSA) cert with the given v3 `extensions`
    /// DER (the inner SEQUENCE OF Extension content; pass `&[]` for none).
    /// Signed by the RSA-512 test key; SPKI is that same key.
    fn build_rsa_cert_with_exts(subject_cn: &str, issuer_cn: &str, extensions: &[u8]) -> Vec<u8> {
        let spki = rsa_spki(RSA512_N, 65537);
        let subject_dn = x509_name(subject_cn);
        let issuer_dn = x509_name(issuer_cn);
        let validity = seq(&[
            tlv(0x17, b"200101000000Z").as_slice(),
            tlv(0x18, b"20990101000000Z").as_slice(),
        ]
        .concat());
        let mut tbs_body = [
            ctx_imp(0, &integer(2)).as_slice(),
            integer(1).as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            issuer_dn.as_slice(),
            validity.as_slice(),
            subject_dn.as_slice(),
            spki.as_slice(),
        ]
        .concat();
        if !extensions.is_empty() {
            // extensions [3] EXPLICIT SEQUENCE OF Extension.
            let ext_seq = seq(extensions);
            let ext_explicit = tlv(0xA3, &ext_seq);
            tbs_body.extend_from_slice(&ext_explicit);
        }
        let tbs = seq(&tbs_body);
        let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
        let mut bs = vec![0u8];
        bs.extend_from_slice(&sig);
        seq(&[
            tbs.as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            tlv(0x03, &bs).as_slice(),
        ]
        .concat())
    }

    /// Build an Extension ::= SEQUENCE { extnID OID, critical BOOL?, extnValue OCTET STRING }.
    fn extension(oid_dotted: &str, critical: bool, value_der: &[u8]) -> Vec<u8> {
        let mut inner = oid(oid_dotted);
        if critical {
            inner.extend_from_slice(&[0x01, 0x01, 0xFF]); // BOOLEAN TRUE
        }
        inner.extend_from_slice(&octet(value_der));
        seq(&inner)
    }

    /// BasicConstraints ::= SEQUENCE { cA BOOLEAN DEFAULT FALSE, ... }.
    /// `cA=TRUE` is encoded explicitly; `cA=FALSE` is the canonical empty
    /// SEQUENCE (the DEFAULT is omitted), which decodes to `ca = false`.
    fn basic_constraints(ca: bool) -> Vec<u8> {
        if ca {
            seq(&[0x01, 0x01, 0xFF])
        } else {
            seq(&[])
        }
    }

    /// KeyUsage ::= BIT STRING.  `first_byte` is the big-endian usage byte
    /// (bit 0 = digitalSignature = 0x80; bit 5 = keyCertSign = 0x04).
    /// Encoded canonically (DER) with the unused-bits count set to the
    /// number of trailing zero bits, as `der`'s BitString decoder expects.
    fn key_usage(first_byte: u8) -> Vec<u8> {
        let unused = if first_byte == 0 {
            0
        } else {
            first_byte.trailing_zeros() as u8
        };
        tlv(0x03, &[unused, first_byte])
    }

    #[test]
    fn rfc5280_leaf_keyusage_without_digitalsignature_rejected() {
        use super::*;
        // Leaf with KeyUsage = keyCertSign only (no digitalSignature) →
        // must be rejected for code-signing use.
        let ku = extension("2.5.29.15", true, &key_usage(0x04)); // keyCertSign
        let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &ku);
        let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let mut ts = TrustStore::empty();
        ts.add_anchor_der(root_der);
        let err = verify_chain(&leaf, &[], &ts).expect_err("leaf KU must reject");
        assert_eq!(err, TrustError::KeyUsageViolation);
    }

    #[test]
    fn rfc5280_leaf_with_digitalsignature_and_codesigning_eku_accepted() {
        use super::*;
        // Leaf with digitalSignature KU + codeSigning EKU, chaining to a
        // root that is a proper CA → accepted.
        let ku = extension("2.5.29.15", true, &key_usage(0x80)); // digitalSignature
        let eku_val = seq(&oid("1.3.6.1.5.5.7.3.3")); // id-kp-codeSigning
        let eku = extension("2.5.29.37", false, &eku_val);
        let leaf_exts = [ku.as_slice(), eku.as_slice()].concat();
        let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &leaf_exts);

        // Root: CA + keyCertSign.
        let root_bc = extension("2.5.29.19", true, &basic_constraints(true));
        let root_ku = extension("2.5.29.15", true, &key_usage(0x04)); // keyCertSign
        let root_exts = [root_bc.as_slice(), root_ku.as_slice()].concat();
        let root_der = build_rsa_cert_with_exts("Root", "Root", &root_exts);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let mut ts = TrustStore::empty();
        ts.add_anchor_der(root_der);
        verify_chain(&leaf, &[], &ts).expect("compliant code-signing chain must verify");
    }

    #[test]
    fn rfc5280_intermediate_with_ca_false_rejected() {
        use super::*;
        // Intermediate carries BasicConstraints cA=FALSE but is used to
        // certify the leaf → BasicConstraintsViolation.
        let leaf_der = build_rsa_cert_with_exts("Leaf", "Inter", &[]);
        let int_bc = extension("2.5.29.19", true, &basic_constraints(false));
        let int_der = build_rsa_cert_with_exts("Inter", "Root", &int_bc);
        let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let int_cert = X509Cert::parse(&int_der).expect("parse int");
        let mut ts = TrustStore::empty();
        ts.add_anchor_der(root_der);
        let err = verify_chain(&leaf, std::slice::from_ref(&int_cert), &ts)
            .expect_err("cA=false intermediate must reject");
        assert_eq!(err, TrustError::BasicConstraintsViolation);
    }

    #[test]
    fn rfc5280_unknown_critical_extension_rejected() {
        use super::*;
        // Leaf carries a bogus *critical* extension we do not process →
        // UnknownCriticalExtension (fail-closed, RFC 5280 §6.1.4(f)).
        let bogus = extension("1.2.3.4.5.6.7", true, &[0x01, 0x02]);
        let leaf_der = build_rsa_cert_with_exts("Leaf", "Root", &bogus);
        let root_der = build_rsa_cert_with_exts("Root", "Root", &[]);

        let leaf = X509Cert::parse(&leaf_der).expect("parse leaf");
        let mut ts = TrustStore::empty();
        ts.add_anchor_der(root_der);
        let err = verify_chain(&leaf, &[], &ts).expect_err("unknown critical ext must reject");
        assert_eq!(err, TrustError::UnknownCriticalExtension);
    }

    // ---------------------------------------------------------------------
    // Task #40 — JKS trust-store parsing tests.
    // ---------------------------------------------------------------------

    /// Hand-assemble a minimal JKS file (version 2) with a single
    /// TrustedCertEntry carrying `cert_der`, MAC-sealed under `password`.
    fn build_jks(cert_der: &[u8], password: &[u8]) -> Vec<u8> {
        fn u16(v: u16, o: &mut Vec<u8>) {
            o.extend_from_slice(&v.to_be_bytes());
        }
        fn u32(v: u32, o: &mut Vec<u8>) {
            o.extend_from_slice(&v.to_be_bytes());
        }
        fn u64(v: u64, o: &mut Vec<u8>) {
            o.extend_from_slice(&v.to_be_bytes());
        }
        let mut body = Vec::new();
        u32(super::JKS_MAGIC, &mut body);
        u32(2, &mut body); // version
        u32(1, &mut body); // entry_count
        u32(2, &mut body); // tag = TrustedCertEntry
        let alias = b"testanchor";
        u16(alias.len() as u16, &mut body);
        body.extend_from_slice(alias);
        u64(0, &mut body); // creation_date_ms
        let ctype = b"X.509";
        u16(ctype.len() as u16, &mut body);
        body.extend_from_slice(ctype);
        u32(cert_der.len() as u32, &mut body);
        body.extend_from_slice(cert_der);
        // Append MAC.
        let mac = super::jks_password_mac(password, &body);
        let mut out = body;
        out.extend_from_slice(&mac);
        out
    }

    #[test]
    fn jks_round_trip_extracts_trusted_cert() {
        let root_subject_dn = x509_name("JksRoot");
        let cert = build_x509_cert("JksRoot", "JksRoot", &root_subject_dn);
        let pw = b"changeit";
        let jks = build_jks(&cert, pw);

        let ders = super::parse_jks_trusted_certs(&jks, pw).expect("jks parse");
        assert_eq!(ders.len(), 1);
        assert_eq!(ders[0], cert);

        // Loading it as a trust store anchor must work.
        let mut ts = TrustStore::empty();
        let n = ts.extend_from_anchors(ders);
        assert_eq!(n, 1);
        assert_eq!(ts.anchor_count(), 1);
    }

    #[test]
    fn jks_wrong_password_rejected() {
        let dn = x509_name("JksRoot");
        let cert = build_x509_cert("JksRoot", "JksRoot", &dn);
        let jks = build_jks(&cert, b"changeit");
        assert!(super::parse_jks_trusted_certs(&jks, b"wrongpw").is_err());
    }

    #[test]
    fn jks_tampered_body_rejected() {
        let dn = x509_name("JksRoot");
        let cert = build_x509_cert("JksRoot", "JksRoot", &dn);
        let mut jks = build_jks(&cert, b"changeit");
        // Flip a byte in the body (not the trailing MAC).
        jks[12] ^= 0xFF;
        assert!(super::parse_jks_trusted_certs(&jks, b"changeit").is_err());
    }

    #[test]
    fn validity_dates_reject_expired_cert() {
        // notAfter in the past must fail cert_dates_ok.
        let der = {
            let subject_dn = x509_name("ExpiredLeaf");
            let issuer_dn = x509_name("ExpiredLeaf");
            let version = ctx_imp(0, &integer(2));
            let serial = integer(1);
            let sig_alg = algorithm_identifier(OID_STUB_SIG);
            // notBefore 2000, notAfter 2001 — both in the past.
            let validity = seq(&[
                tlv(0x17, b"000101000000Z").as_slice(),
                tlv(0x17, b"010101000000Z").as_slice(),
            ]
            .concat());
            let spki = x509_spki(&subject_dn);
            let tbs = seq(&[
                version.as_slice(),
                serial.as_slice(),
                sig_alg.as_slice(),
                issuer_dn.as_slice(),
                validity.as_slice(),
                subject_dn.as_slice(),
                spki.as_slice(),
            ]
            .concat());
            let mut buf = tbs.clone();
            buf.extend_from_slice(&subject_dn);
            let sig = sha256::digest(&buf);
            let mut bs = vec![0u8];
            bs.extend_from_slice(&sig);
            seq(&[
                tbs.as_slice(),
                algorithm_identifier(OID_STUB_SIG).as_slice(),
                tlv(0x03, &bs).as_slice(),
            ]
            .concat())
        };
        let cert = X509Cert::parse(&der).expect("parse expired cert");
        assert!(
            !super::cert_dates_ok(&cert),
            "year-2001 notAfter must be expired"
        );
    }

    // -----------------------------------------------------------------
    // V1 — per-entry manifest digest binding helpers.
    // -----------------------------------------------------------------

    /// Minimal standard base64 encoder for the manifest-digest tests.
    fn b64enc(input: &[u8]) -> String {
        const ALPHA: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHA[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHA[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn sf_binds_manifest_accepts_matching_digest() {
        let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
        let mdigest = b64enc(&raw_digest(DigestAlg::Sha256, manifest));
        let sf = format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {mdigest}\r\n\r\n");
        assert!(verify_sf_binds_manifest(sf.as_bytes(), manifest));
    }

    #[test]
    fn sf_binds_manifest_rejects_tampered_manifest() {
        let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
        let mdigest = b64enc(&raw_digest(DigestAlg::Sha256, manifest));
        let sf = format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {mdigest}\r\n\r\n");
        let tampered = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: zzz\r\n";
        assert!(!verify_sf_binds_manifest(sf.as_bytes(), tampered));
    }

    #[test]
    fn sf_binds_manifest_fails_closed_without_digest_manifest() {
        // A `.SF` that only commits to the main attributes (not the whole
        // manifest) must NOT be accepted as binding the manifest.
        let manifest = b"Manifest-Version: 1.0\r\n\r\nName: a/B.class\r\nSHA-256-Digest: xyz\r\n";
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest-Main-Attributes: AAAA\r\n\r\n";
        assert!(!verify_sf_binds_manifest(sf, manifest));
    }

    #[test]
    fn parse_manifest_entry_digests_extracts_each_section() {
        let body = b"hello world contents";
        let dig = b64enc(&raw_digest(DigestAlg::Sha256, body));
        let manifest = format!(
            "Manifest-Version: 1.0\r\n\r\n\
             Name: pkg/Foo.class\r\nSHA-256-Digest: {dig}\r\n\r\n\
             Name: pkg/dir/\r\n\r\n"
        );
        let parsed = parse_manifest_entry_digests(manifest.as_bytes());
        // Directory section (no digest) is dropped; only Foo.class remains.
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "pkg/Foo.class");
        assert_eq!(parsed[0].alg, DigestAlg::Sha256);
        assert!(digest_matches(parsed[0].alg, body, &parsed[0].expected));
        // A different body must NOT match the recorded digest.
        assert!(!digest_matches(
            parsed[0].alg,
            b"tampered",
            &parsed[0].expected
        ));
    }

    #[test]
    fn parse_manifest_entry_digests_picks_strongest_alg() {
        let body = b"abc";
        let d1 = b64enc(&raw_digest(DigestAlg::Sha1, body));
        let d256 = b64enc(&raw_digest(DigestAlg::Sha256, body));
        let manifest = format!(
            "Manifest-Version: 1.0\r\n\r\n\
             Name: X.class\r\nSHA1-Digest: {d1}\r\nSHA-256-Digest: {d256}\r\n\r\n"
        );
        let parsed = parse_manifest_entry_digests(manifest.as_bytes());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].alg, DigestAlg::Sha256, "strongest digest wins");
        assert!(digest_matches(parsed[0].alg, body, &parsed[0].expected));
    }

    // ---------------------------------------------------------------------
    // TRUST BOUNDARY — the three-valued result, and what "verified" covers.
    //
    // These pin the contract written up in
    // `docs/security/signed-jar-trust.md`:
    //
    //   * "we checked and it failed" (`SigVerify::Bad`) and "we never
    //     checked" (`SigVerify::Unsupported`) must stay distinguishable, and
    //     both must refuse;
    //   * an integrity check must never be readable as a trust check;
    //   * a trust-shaped API must refuse unless every step it claims
    //     actually ran — including for certificates it merely carried.
    //
    // The fixtures use real RSA (the 512-bit test key) so they drive the
    // production verification path, not the `OID_STUB_SIG` test backdoor.
    // ---------------------------------------------------------------------

    /// A validity window that contains "now" — `notBefore` 2020 (UTCTime),
    /// `notAfter` 2099 (GeneralizedTime), matching the encodings real CAs
    /// emit either side of the 2050 boundary.
    fn tb_validity_now() -> Vec<u8> {
        seq(&[
            tlv(0x17, b"200101000000Z").as_slice(),
            tlv(0x18, b"20990101000000Z").as_slice(),
        ]
        .concat())
    }

    /// A real `sha256WithRSAEncryption` certificate signed by (and carrying
    /// the SPKI of) the RSA-512 test key, with a caller-chosen `validity`
    /// SEQUENCE. Self-signed when `subject_cn == issuer_cn`.
    fn tb_cert(subject_cn: &str, issuer_cn: &str, validity: &[u8]) -> Vec<u8> {
        let spki = rsa_spki(RSA512_N, 65537);
        let subject_dn = x509_name(subject_cn);
        let issuer_dn = x509_name(issuer_cn);
        let tbs = seq(&[
            ctx_imp(0, &integer(2)).as_slice(),
            integer(1).as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            issuer_dn.as_slice(),
            validity,
            subject_dn.as_slice(),
            spki.as_slice(),
        ]
        .concat());
        let sig = rsa_sign(&tbs, DigestAlg::Sha256, RSA512_N, RSA512_D);
        let mut bs = vec![0u8];
        bs.extend_from_slice(&sig);
        seq(&[
            tbs.as_slice(),
            algorithm_identifier("1.2.840.113549.1.1.11").as_slice(),
            tlv(0x03, &bs).as_slice(),
        ]
        .concat())
    }

    /// A real RSA-signed CMS signer block over `sf`. The `certificates` set
    /// is the leaf followed by `extra_certs` verbatim — the hook for the
    /// "attacker appends a cert" case. `signer_sig_alg_oid` names the
    /// SignerInfo `signatureAlgorithm`. Returns `(block, leaf_der, root_der)`.
    fn tb_signer_block_with_alg(
        sf: &[u8],
        extra_certs: &[Vec<u8>],
        signer_sig_alg_oid: &str,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let validity = tb_validity_now();
        let leaf_der = tb_cert("TB-Leaf", "TB-Root", &validity);
        let root_der = tb_cert("TB-Root", "TB-Root", &validity);

        let dig = sha256::digest(sf).to_vec();
        let attr_ct = seq(&[
            oid(OID_CONTENT_TYPE).as_slice(),
            set(&oid(OID_DATA)).as_slice(),
        ]
        .concat());
        let attr_md = seq(&[
            oid(OID_MESSAGE_DIGEST).as_slice(),
            set(&octet(&dig)).as_slice(),
        ]
        .concat());
        let attrs_inner = [attr_ct.as_slice(), attr_md.as_slice()].concat();
        // RFC 5652 §5.4: the signature covers the explicit SET OF form.
        let signed_attrs_der = set(&attrs_inner);
        let si_sig = rsa_sign(&signed_attrs_der, DigestAlg::Sha256, RSA512_N, RSA512_D);
        let auth_attrs = ctx_imp(0, &attrs_inner);

        let signer_info = seq(&[
            integer(1).as_slice(),
            issuer_and_serial("TB-Root", 1).as_slice(),
            algorithm_identifier(OID_SHA256).as_slice(),
            auth_attrs.as_slice(),
            algorithm_identifier(signer_sig_alg_oid).as_slice(),
            octet(&si_sig).as_slice(),
        ]
        .concat());

        let mut certs_concat = leaf_der.clone();
        for c in extra_certs {
            certs_concat.extend_from_slice(c);
        }
        let signed_data = seq(&[
            integer(1).as_slice(),
            set(&algorithm_identifier(OID_SHA256)).as_slice(),
            seq(&oid(OID_DATA)).as_slice(),
            ctx_imp(0, &certs_concat).as_slice(),
            set(&signer_info).as_slice(),
        ]
        .concat());
        let block = seq(&[
            oid(OID_SIGNED_DATA).as_slice(),
            ctx_imp(0, &signed_data).as_slice(),
        ]
        .concat());
        (block, leaf_der, root_der)
    }

    /// [`tb_signer_block_with_alg`] with the ordinary `sha256WithRSA` OID.
    fn tb_signer_block(sf: &[u8], extra_certs: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        tb_signer_block_with_alg(sf, extra_certs, "1.2.840.113549.1.1.11")
    }

    #[test]
    fn tb_unsupported_signature_algorithm_is_unsupported_not_bad() {
        use super::{verify_signature_with_spki, SigVerify};
        let spki = rsa_spki(RSA1024_N, 65537);
        let msg = b"payload";
        let sig = rsa_sign(msg, DigestAlg::Sha256, RSA1024_N, RSA1024_D);

        // RSASSA-PSS (1.2.840.113549.1.1.10) is a *different padding scheme*
        // and is deliberately absent from `sig_alg_digest` — verifying it
        // with the PKCS#1 v1.5 verifier would be the wrong operation.
        let r = verify_signature_with_spki(&spki, "1.2.840.113549.1.1.10", msg, &sig);
        assert_eq!(
            r,
            SigVerify::Unsupported,
            "an algorithm we do not implement must report that we did not check"
        );
        assert_ne!(
            r,
            SigVerify::Bad,
            "reporting 'unsupported' as 'the signature is bad' claims a verdict \
             that was never reached — that is the ambiguity this enum removes"
        );

        // Recognised OID, wrong key family: an ECDSA algorithm against an
        // RSA key. There is no verification to perform.
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.10045.4.3.2", msg, &sig),
            SigVerify::Unsupported
        );
        // An OID we have never heard of.
        assert_eq!(
            verify_signature_with_spki(&spki, "1.1.1.1", msg, &sig),
            SigVerify::Unsupported
        );
        // An SPKI that will not parse is likewise "not checked", not "failed".
        assert_eq!(
            verify_signature_with_spki(b"\x30\x03not-a-key", "1.2.840.113549.1.1.11", msg, &sig),
            SigVerify::Unsupported
        );
    }

    #[test]
    fn tb_unsupported_algorithm_is_rejected_at_the_trust_api() {
        // The distinction has to survive all the way out, and `Unsupported`
        // must refuse just as hard as `Bad` does.
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
        let (block, _leaf, root) = tb_signer_block_with_alg(sf, &[], "1.2.840.113549.1.1.10");

        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root));
        assert!(
            verify_signer_block(&block, sf, &ts).is_none(),
            "a signer block whose algorithm we cannot verify must never be accepted"
        );

        // ...and the refusal says so, rather than accusing the JAR of
        // carrying a bad signature.
        let err = super::parse_signed_data(&block, sf, true)
            .expect_err("unsupported signer algorithm must refuse");
        assert!(
            err.contains("could not be verified"),
            "expected an 'unverifiable' diagnosis, got: {err}"
        );
        assert!(
            !err.contains("does not verify"),
            "an unsupported algorithm must not be reported as a failed verification: {err}"
        );
    }

    #[test]
    fn tb_genuine_signature_mismatch_is_bad_not_unsupported() {
        use super::{verify_signature_with_spki, SigVerify};
        // PRESERVED NEGATIVE: when a verifier really runs and says no, that
        // is a security decision and must stay one. Turning this into
        // `Unsupported` would be just as wrong in the other direction.
        let spki = rsa_spki(RSA1024_N, 65537);
        let msg = b"the real message";
        let sig = rsa_sign(msg, DigestAlg::Sha256, RSA1024_N, RSA1024_D);

        let other = b"a different message";
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", other, &sig),
            SigVerify::Bad,
            "a completed verification that fails is a genuine negative"
        );

        // Corrupt signature bits keep the correct length, so the encoding
        // guard cannot fire — this is exactly what a forgery looks like.
        let mut corrupt = sig.clone();
        corrupt[5] ^= 0xFF;
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", msg, &corrupt),
            SigVerify::Bad
        );

        // And the same input still verifies untouched, so the assertions
        // above are not passing for an unrelated reason.
        assert_eq!(
            verify_signature_with_spki(&spki, "1.2.840.113549.1.1.11", msg, &sig),
            SigVerify::Ok
        );
    }

    #[test]
    fn tb_unsupported_and_bad_stay_distinct_trust_errors() {
        let validity = tb_validity_now();
        let parent_der = tb_cert("TB-Root", "TB-Root", &validity);
        let parent = X509Cert::parse(&parent_der).expect("parent parses");

        // Same key, tampered signature bytes → a verifier ran and said no.
        let mut bad_der = tb_cert("TB-Leaf", "TB-Root", &validity);
        let last = bad_der.len() - 1;
        bad_der[last] ^= 0xFF;
        let bad = X509Cert::parse(&bad_der).expect("tampered cert still parses");
        assert_eq!(
            bad.link_signature_ok(&parent)
                .expect_err("tampered link must reject"),
            TrustError::BadSignature
        );

        // A parent whose SPKI names a key type we do not carry → nothing
        // was verified → `NotImplemented`, which a caller matching on
        // `TrustError` cannot confuse with `BadSignature`.
        let stub_parent_der = build_x509_cert("TB-Root", "TB-Root", &x509_name("TB-Root"));
        let stub_parent = X509Cert::parse(&stub_parent_der).expect("stub parent parses");
        let child_der = tb_cert("TB-Leaf", "TB-Root", &validity);
        let child = X509Cert::parse(&child_der).expect("child parses");
        assert_eq!(
            child
                .link_signature_ok(&stub_parent)
                .expect_err("unverifiable link must reject"),
            TrustError::NotImplemented
        );
    }

    #[test]
    fn tb_self_consistent_signature_without_an_anchor_is_not_trusted() {
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
        let (block, leaf_der, root_der) = tb_signer_block(sf, &[]);

        // The block IS internally consistent — with its root as an anchor it
        // verifies. Establishing that first is what keeps the negative below
        // from passing vacuously.
        let mut anchored = TrustStore::empty();
        assert!(anchored.add_anchor_der(root_der.clone()));
        assert!(
            verify_signer_block(&block, sf, &anchored).is_some(),
            "fixture must be a genuinely valid signature"
        );

        // Identical bytes, no anchor: a valid signature by an unknown party
        // establishes nothing. This is the whole point of a trust anchor —
        // anyone can produce a self-consistent signed JAR.
        assert!(
            verify_signer_block(&block, sf, &TrustStore::empty()).is_none(),
            "a cryptographically valid signature with no path to an anchor \
             must not be reported as a verified signer"
        );

        // The reason is specifically the missing anchor, not a parse accident.
        let leaf = X509Cert::parse(&leaf_der).expect("leaf parses");
        assert_eq!(
            verify_chain(&leaf, &[], &TrustStore::empty())
                .expect_err("empty store must find no path"),
            TrustError::NoTrustAnchor
        );
    }

    #[test]
    fn tb_unvalidated_certificates_are_not_surfaced_as_the_signers() {
        // A CMS `certificates` set is attacker-supplied. Appending a
        // certificate to an otherwise legitimately signed JAR must not get
        // it reported through `CodeSource.getCertificates()`: it took no
        // part in path construction, so nothing about it was verified.
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: AAAA\r\n\r\n";
        let interloper = tb_cert("TB-Interloper", "TB-Interloper", &tb_validity_now());
        let (block, leaf_der, root_der) = tb_signer_block(sf, &[interloper.clone()]);

        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root_der.clone()));
        let vs =
            verify_signer_block(&block, sf, &ts).expect("the legitimate signer must still verify");

        assert_eq!(
            vs.chain,
            vec![leaf_der, root_der],
            "chain must be exactly the validated path, leaf first, anchor last"
        );
        assert!(
            !vs.chain.contains(&interloper),
            "a certificate that was never validated was surfaced as the signer's"
        );
    }

    #[test]
    fn tb_expired_and_not_yet_valid_certs_reject() {
        let in_date = tb_validity_now();
        let root_der = tb_cert("TB-Root", "TB-Root", &in_date);
        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root_der));

        // notAfter in the past.
        let expired = tb_cert(
            "TB-Leaf",
            "TB-Root",
            &seq(&[
                tlv(0x17, b"000101000000Z").as_slice(),
                tlv(0x17, b"010101000000Z").as_slice(),
            ]
            .concat()),
        );
        let cert = X509Cert::parse(&expired).expect("expired cert parses");
        assert_eq!(
            verify_chain(&cert, &[], &ts).expect_err("expired leaf must reject"),
            TrustError::Expired
        );

        // notBefore in the future.
        let premature = tb_cert(
            "TB-Leaf",
            "TB-Root",
            &seq(&[
                tlv(0x18, b"20900101000000Z").as_slice(),
                tlv(0x18, b"20990101000000Z").as_slice(),
            ]
            .concat()),
        );
        let cert = X509Cert::parse(&premature).expect("not-yet-valid cert parses");
        assert_eq!(
            verify_chain(&cert, &[], &ts).expect_err("not-yet-valid leaf must reject"),
            TrustError::Expired
        );

        // The same fixture with a window covering "now" chains fine, so the
        // two rejections above are about the dates and nothing else.
        let good = tb_cert("TB-Leaf", "TB-Root", &in_date);
        let cert = X509Cert::parse(&good).expect("in-date cert parses");
        verify_chain(&cert, &[], &ts).expect("in-date leaf must chain to the anchor");
    }

    #[test]
    fn tb_unreadable_validity_window_rejects() {
        // RFC 5280 §4.1.2.5 admits only UTCTime and GeneralizedTime. A
        // validity SEQUENCE carrying something else is structurally
        // parseable, so it used to reach `cert_dates_ok` — which answered
        // "in date" for a window it could not read. A window we could not
        // read is a window we did not check, and it now refuses.
        let bogus = seq(&[
            tlv(0x0C, b"whenever").as_slice(), // UTF8String, not a Time
            tlv(0x0C, b"eventually").as_slice(),
        ]
        .concat());
        let der = tb_cert("TB-Leaf", "TB-Root", &bogus);
        let cert = X509Cert::parse(&der).expect("structurally parseable");
        assert!(
            !super::cert_dates_ok(&cert),
            "an unreadable validity window must not be reported as in-date"
        );

        // ...and it is fatal to the chain, not merely logged.
        let root_der = tb_cert("TB-Root", "TB-Root", &tb_validity_now());
        let mut ts = TrustStore::empty();
        assert!(ts.add_anchor_der(root_der));
        assert_eq!(
            verify_chain(&cert, &[], &ts).expect_err("must reject"),
            TrustError::Expired
        );
    }

    #[test]
    fn tb_entry_absent_from_manifest_is_not_covered() {
        // The classic partial-signing gap: an archive holds entries the
        // manifest never named. `parse_manifest_entry_digests` is the
        // authority on what the signer committed to, and it must simply not
        // list them — "absent" is the signal callers gate on.
        let signed_body = b"class bytes that were signed";
        let dig = b64enc(&raw_digest(DigestAlg::Sha256, signed_body));
        // The archive also contains `pkg/Injected.class` (no section at all)
        // and `pkg/NoDigest.class` (a section carrying no digest).
        let manifest = format!(
            "Manifest-Version: 1.0\r\n\r\n\
             Name: pkg/Signed.class\r\nSHA-256-Digest: {dig}\r\n\r\n\
             Name: pkg/NoDigest.class\r\nComment: not a digest attribute\r\n\r\n"
        );
        let declared = parse_manifest_entry_digests(manifest.as_bytes());
        let names: Vec<&str> = declared.iter().map(|d| d.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["pkg/Signed.class"],
            "only entries the manifest commits to with a digest are covered"
        );
        assert!(
            !names.contains(&"pkg/Injected.class"),
            "an entry with no manifest section must never be treated as covered"
        );
        assert!(
            !names.contains(&"pkg/NoDigest.class"),
            "a Name: section without a recognised <alg>-Digest commits to nothing"
        );
        // Guard against the whole assertion set passing because the parser
        // returned nothing at all: the one covered entry really is covered.
        assert!(digest_matches(
            declared[0].alg,
            signed_body,
            &declared[0].expected
        ));
    }

    #[test]
    fn tb_integrity_checking_still_works() {
        // The integrity half of the JAR chain — .SF → MANIFEST.MF → entry
        // bytes — is genuinely complete, and this audit preserves it rather
        // than rejecting it. It is named as integrity, not trust.
        let body = b"the real class bytes";
        let entry_digest = b64enc(&raw_digest(DigestAlg::Sha256, body));
        let manifest = format!(
            "Manifest-Version: 1.0\r\n\r\n\
             Name: pkg/Real.class\r\nSHA-256-Digest: {entry_digest}\r\n\r\n"
        );
        let manifest_digest = b64enc(&raw_digest(DigestAlg::Sha256, manifest.as_bytes()));
        let sf =
            format!("Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: {manifest_digest}\r\n\r\n");

        assert!(
            verify_sf_binds_manifest(sf.as_bytes(), manifest.as_bytes()),
            ".SF must bind the manifest it committed to"
        );
        let declared = parse_manifest_entry_digests(manifest.as_bytes());
        assert_eq!(declared.len(), 1);
        assert!(
            digest_matches(declared[0].alg, body, &declared[0].expected),
            "the committed-to entry's bytes must match"
        );

        // Tamper at either link and it breaks.
        assert!(!digest_matches(
            declared[0].alg,
            b"swapped bytes",
            &declared[0].expected
        ));
        let tampered = manifest.replace("pkg/Real.class", "pkg/Evil.class");
        assert!(!verify_sf_binds_manifest(
            sf.as_bytes(),
            tampered.as_bytes()
        ));

        // TRUST BOUNDARY: everything above used no key and no certificate.
        // A `.SF` anyone can write produces the same `true`, so integrity is
        // not, and must not be read as, trust — that comes only from
        // `verify_signer_block` against a real anchor.
        assert!(
            verify_signer_block(sf.as_bytes(), sf.as_bytes(), &TrustStore::empty()).is_none(),
            "manifest integrity says nothing about who produced the bytes"
        );
    }
}
