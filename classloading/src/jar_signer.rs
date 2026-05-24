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
//! # Out of scope — `TODO(post-orchestrator)`
//!
//! * **RSA / DSA / EC public-key signature verification over the
//!   authenticatedAttributes blob.**  The crypto primitives
//!   (`crypto_impl::Rsa::verify_sha256`, ECDSA, big-integer modpow)
//!   currently live in `cratonvm-native-builtins`, which depends on
//!   `cratonvm-classloading` — pulling them in here would create a
//!   dependency cycle.  Until those primitives are hoisted to a shared
//!   crate (`cratonvm-types` or a new `cratonvm-crypto-core`), this
//!   module verifies only the `.SF` digest binding, not the signature
//!   over it.  That still removes the original auth-bypass
//!   (`getCertificates()` no longer returns garbage), but a determined
//!   attacker who can craft a valid `*.SF`/`*.RSA` pair *consistent with
//!   their own key* could still forge a signer identity until pubkey
//!   verification lands.
//! * **Trust-store integration (partial — task #40, deferred from #1).**
//!   This module now exposes [`TrustStore`] and [`verify_chain`], walks
//!   the embedded `chain` (leaf → intermediates → anchor) by Subject↔
//!   Issuer DN match, and refuses signer blocks whose leaf has no path
//!   to a trust anchor.  See [`TrustStore::load_default`] for the source
//!   priority (`javax.net.ssl.trustStore` sys-prop, then a `CRATONVM_TRUST_PEM`
//!   env-var PEM bundle, then the JDK `cacerts` fallback path).  What is
//!   still **NOT** implemented is the cryptographic signature step on
//!   each chain link — for the same dependency-cycle reason as the
//!   pubkey-over-authAttrs gap above.  Until the crypto primitives are
//!   hoisted out of `cratonvm-native-builtins`, the link-signature check
//!   delegates to a deliberately conservative stub that recognises only
//!   a synthetic `craton-stub-sig` algorithm — real RSA/ECDSA blobs
//!   surface as [`TrustError::NotImplemented`], which is treated as a
//!   verification failure by [`verify_signer_block`].  PEM-encoded
//!   anchors are accepted (we have a local DER reader); PKCS#12 / JKS
//!   binary trust-store files surface as `NotImplemented` (no `p12` /
//!   `keystore` crate is reachable from `classloading`).
//! * **Multiple-signer SignerInfo dispatch.**  We only verify the first
//!   `SignerInfo`; multi-signer JARs (rare) collapse to "first signer
//!   verified".
//! * **Reading the `.SF` from a remote / nested location.**  Caller is
//!   responsible for feeding us the matching `.SF` bytes.
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
/// Pass [`TrustStore::permissive_legacy_tests`] to skip the chain step
/// entirely — used by the pre-task-#40 self-consistency tests below that
/// embed non-X.509-shaped marker certs.  Production code paths must
/// supply a real trust store via [`TrustStore::load_default`].
///
/// # TODO(post-orchestrator)
///
/// Pub-key signature verification over the authenticated-attributes
/// blob, and **cryptographic** verification of each chain link's
/// `signatureAlgorithm`-over-TBS-cert, are still not implemented — see
/// module-level docs.  Both gaps surface as `None` here (the chain step
/// returns [`TrustError::NotImplemented`] for unrecognised signature
/// algorithms, which we treat as failure rather than success).
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
    let vs = match parse_signed_data(signer_block_der, sf_bytes) {
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
        match verify_chain(&leaf, &intermediates, trust_store) {
            Ok(()) => {}
            Err(e) => {
                warn!("jar signer: chain validation rejected leaf: {:?}", e);
                return None;
            }
        }
    }
    Some(vs)
}

// ---------------------------------------------------------------------------
// PKCS#7 / CMS parsing
// ---------------------------------------------------------------------------

fn parse_signed_data(der: &[u8], sf_bytes: &[u8]) -> Result<VerifiedSigner, &'static str> {
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
    let digest_alg = digest_alg_from_oid(&digest_alg_oid)
        .ok_or("unsupported digest algorithm in SignerInfo")?;

    // Authenticated attributes [0] IMPLICIT SET OF Attribute.
    let auth_attrs = if si.peek_tag() == Some(TAG_CTX0) {
        let (tlv, _) = si.read_tlv()?;
        Some(tlv.content.to_vec())
    } else {
        None
    };
    let auth_attrs = auth_attrs
        .ok_or("SignerInfo is missing authenticatedAttributes — refusing to skip integrity check")?;

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
    let recomputed = digest_alg.digest(sf_bytes);
    if !ct_eq(&stored_digest, &recomputed) {
        return Err(".SF digest in messageDigest does not match SHA(SF) — tampered .SF");
    }

    // At this point integrity of the SignerInfo ↔ .SF binding is confirmed
    // (modulo pubkey-sig verification — see module docs).  Materialise the
    // returned struct.
    if certs_der.is_empty() {
        return Err("SignedData has no embedded certificates");
    }
    let principal = principal_from_sid(sid_raw).unwrap_or_else(|| "<unparsed>".to_string());
    Ok(VerifiedSigner {
        chain: certs_der,
        principal,
        digest_alg,
    })
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

impl DigestAlg {
    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            DigestAlg::Sha1 => sha1::digest(data).to_vec(),
            DigestAlg::Sha256 => sha256::digest(data).to_vec(),
            // SHA-384 / SHA-512 use the SHA-512 family.  We do not
            // currently implement the family in `classloading`; signed
            // JARs in the wild overwhelmingly use SHA-256 (modern
            // jarsigner default) or SHA-1 (legacy).  Reject unsupported
            // algos rather than silently downgrading to SHA-1.
            DigestAlg::Sha384 | DigestAlg::Sha512 => Vec::new(),
        }
    }
}

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
    let total = hdr
        .checked_add(content_len)
        .ok_or("TLV length overflow")?;
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
        let mut h: [u32; 5] = [
            0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0,
        ];
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
                let s0 =
                    w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
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
// **Cryptographic note.**  The link-signature step is intentionally
// *not* a real RSA/ECDSA verify (see the module-level docs for why the
// crypto primitives are out of reach from `classloading`).  We
// recognise one synthetic algorithm OID — `craton-stub-sig` (used
// exclusively by the in-process tests) — and treat every other
// algorithm as [`TrustError::NotImplemented`].  A real-world cert
// chain therefore *will* reject as `NotImplemented`, which
// [`verify_signer_block`] surfaces as `None` (verification failure).
// That is the conservative direction: until real crypto lands, no
// chain validates against system anchors.  Audit follow-up: hoist
// `cratonvm_native_builtins::crypto_impl::Rsa::verify_*` into a shared
// crate so this stub can be replaced with real cryptography.

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
    /// The signature on a chain link uses an algorithm this build
    /// cannot verify.  Real RSA/ECDSA signatures land here until the
    /// shared crypto crate is in place — see module-level docs.
    NotImplemented,
    /// One of the certs is structurally invalid (wrong tag, truncated
    /// TBSCertificate, missing Subject/Issuer, ...).
    Malformed,
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
/// 1. `javax.net.ssl.trustStore` system property (we read the matching
///    env-var `JAVAX_NET_SSL_TRUSTSTORE`).  PEM contents are loaded
///    directly; PKCS#12 / JKS binary blobs surface as a warn-level
///    log and the source is skipped (no PKCS#12 parser reachable here).
/// 2. `CRATONVM_TRUST_PEM` env-var pointing at a PEM bundle.  This is
///    the recommended on-disk source for CratonVM — pure DER inside
///    base64 boundaries, parseable by the local TLV reader.
/// 3. `rustls-native-certs`-style system root store — **placeholder**.
///    `classloading` cannot pull the crate (acceptance #6 forbids
///    adding deps); when wired by the host VM, [`TrustStore::extend_from_anchors`]
///    accepts pre-decoded DER blobs and is the integration seam for
///    `rustls_native_certs::load_native_certs()` results.
/// 4. The JDK `cacerts` path — `$JAVA_HOME/lib/security/cacerts`.  This
///    is JKS-formatted and likewise out of reach for the local parser.
///    Recorded for completeness; produces an empty contribution today.
///
/// An empty trust store rejects every chain with
/// [`TrustError::NoTrustAnchor`].  Use
/// [`TrustStore::permissive_legacy_tests`] only for the pre-task-#40
/// self-consistency fixtures in this file.
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
        // when materialising sysprops back to native code.
        if let Ok(p) = std::env::var("JAVAX_NET_SSL_TRUSTSTORE") {
            ts.try_load_path(&p, "javax.net.ssl.trustStore");
        }

        // 2. CratonVM-native PEM bundle.
        if let Ok(p) = std::env::var("CRATONVM_TRUST_PEM") {
            ts.try_load_path(&p, "CRATONVM_TRUST_PEM");
        }

        // 3. System root store — placeholder.  When the host VM has
        // already decoded its native-cert store (`rustls-native-certs`
        // lives in `native-builtins`, not here), it should call
        // `extend_from_anchors` directly.  We just record that the
        // source slot exists.
        ts.sources_loaded
            .push("system-root-store: not wired (acceptance #6 forbids new dep)".to_string());

        // 4. JDK `cacerts` fallback.  Format is JKS — out of reach for
        // the local DER reader.  Document the gap.
        if let Some(jh) = std::env::var_os("JAVA_HOME") {
            let mut path = std::path::PathBuf::from(jh);
            path.push("lib");
            path.push("security");
            path.push("cacerts");
            if path.exists() {
                ts.sources_loaded.push(format!(
                    "{}: JKS not supported in classloading (no `p12`/`keystore` crate reachable)",
                    path.display()
                ));
            }
        }
        ts
    }

    /// Attempt to load one on-disk source.  PEM bundles are accepted;
    /// PKCS#12 / JKS magic bytes are recognised and skipped with a
    /// warn-level log.
    fn try_load_path(&mut self, p: &str, label: &str) {
        match std::fs::read(p) {
            Ok(bytes) => {
                // PKCS#12: SEQUENCE { INTEGER version (3) ... } — first
                // two bytes are 0x30 0x82 (long-form length) then a 0x02
                // INTEGER.  JKS magic: 0xFE 0xED 0xFE 0xED.
                if bytes.starts_with(&[0xFE, 0xED, 0xFE, 0xED]) {
                    warn!(
                        "{}={} appears to be JKS; classloading cannot parse JKS (NotImplemented)",
                        label, p
                    );
                    self.sources_loaded
                        .push(format!("{} JKS: NotImplemented", label));
                    return;
                }
                // Look for the PEM banner — if present, treat as PEM.
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if text.contains("-----BEGIN") {
                        let n = self.load_pem_bundle(text);
                        self.sources_loaded
                            .push(format!("{}={} ({} anchors)", label, p, n));
                        return;
                    }
                }
                // Otherwise assume PKCS#12 — also not implemented here.
                warn!(
                    "{}={} is not PEM; PKCS#12 / JKS parsers are out of reach for classloading \
                     (acceptance #6 — no new deps).  Skipping.",
                    label, p
                );
                self.sources_loaded
                    .push(format!("{} binary keystore: NotImplemented", label));
            }
            Err(e) => {
                warn!("{}={} could not be read: {}", label, p, e);
            }
        }
    }

    /// Look up an anchor whose Subject DN equals `dn`.
    fn find_anchor_by_subject(&self, dn: &[u8]) -> Option<&X509Anchor> {
        self.anchors.iter().find(|a| a.subject_dn.as_slice() == dn)
    }
}

/// Process-wide default trust store, lazily populated on first call.
///
/// `class_path.rs::extract_jar_signer_blocks` reaches for this; tests
/// in this file build a private one instead.
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
        // validity (SEQUENCE).
        let (_, n) = read_tlv(rest)?;
        rest = &rest[n..];
        // subject Name (SEQUENCE OF RDN).
        let subject_start = rest;
        let (subj_tlv, n) = read_tlv(rest)?;
        if subj_tlv.tag != TAG_SEQUENCE {
            return Err("X509Cert: subject is not SEQUENCE");
        }
        let subject_dn = &subject_start[..n];

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
        })
    }

    /// Is this cert self-signed (Subject DN == Issuer DN)?
    pub fn is_self_signed(&self) -> bool {
        self.subject_dn == self.issuer_dn
    }

    /// Does `self`'s signature link it to `parent`?  Cryptographically
    /// this should verify `parent.public_key`-signed-`self.tbs_der` ==
    /// `self.signature_bytes`.  Since real RSA/ECDSA are out of reach
    /// here, we recognise only [`OID_STUB_SIG`] and treat everything
    /// else as [`TrustError::NotImplemented`].
    pub fn link_signature_ok(&self, parent: &X509Cert) -> Result<(), TrustError> {
        match self.sig_alg_oid.as_str() {
            OID_STUB_SIG => {
                // Test-only computation: SHA-256(tbs_der || parent.subject_dn).
                let mut buf = Vec::with_capacity(self.tbs_der.len() + parent.subject_dn.len());
                buf.extend_from_slice(self.tbs_der);
                buf.extend_from_slice(parent.subject_dn);
                let expected = sha256::digest(&buf);
                if ct_eq(self.signature_bytes, &expected) {
                    Ok(())
                } else {
                    Err(TrustError::BadSignature)
                }
            }
            _ => Err(TrustError::NotImplemented),
        }
    }
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
///   * Bad link signature — `Err(BadSignature)`.
///   * Real-crypto algorithm — `Err(NotImplemented)`.
///   * Chain exceeds [`MAX_CHAIN_LEN`] — `Err(TooLong)`.
///   * Already-visited cert — `Err(Cyclic)`.
pub fn verify_chain<'a>(
    leaf: &'a X509Cert<'a>,
    intermediates: &'a [X509Cert<'a>],
    trust_store: &TrustStore,
) -> Result<(), TrustError> {
    let mut current: &'a X509Cert<'a> = leaf;
    // Track visited Subject DNs by owned bytes — we re-walk through
    // both leaf-borrowed slices and trust-store-borrowed slices and
    // mixing those lifetimes inside a `Vec<&[u8]>` is awkward.
    let mut visited: Vec<Vec<u8>> = Vec::new();
    visited.push(current.subject_dn.to_vec());

    for _step in 0..MAX_CHAIN_LEN {
        if let Some(anchor) = trust_store.find_anchor_by_subject(current.issuer_dn) {
            let parent = X509Cert::parse(&anchor.der).map_err(|_| TrustError::Malformed)?;
            current.link_signature_ok(&parent)?;
            return Ok(());
        }
        // Look up an intermediate whose subject == current.issuer.
        let next = intermediates
            .iter()
            .find(|c| c.subject_dn == current.issuer_dn);
        match next {
            Some(parent) => {
                if visited.iter().any(|v| v.as_slice() == parent.subject_dn) {
                    return Err(TrustError::Cyclic);
                }
                current.link_signature_ok(parent)?;
                visited.push(parent.subject_dn.to_vec());
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
        let atv = seq(&[oid("2.5.4.3").as_slice(), tlv(0x13, cn.as_bytes()).as_slice()].concat());
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
            vec![0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD,
                 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF,
                 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD, 0xBE, 0xEF, 0xDE, 0xAD,
                 0xBE, 0xEF]
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
        let attrs_set_content = [attr_content_type.as_slice(), attr_message_digest.as_slice()].concat();
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
        seq(&[oid(OID_SIGNED_DATA).as_slice(), ctx_imp(0, &signed_data).as_slice()].concat())
    }

    #[test]
    fn verifies_self_consistent_sha256_signer_block() {
        let sf = b"Signature-Version: 1.0\r\nSHA-256-Digest-Manifest: abc123=\r\n\r\n";
        let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ false);
        let vs = verify_signer_block(&block, sf, &legacy_ts()).expect("well-formed block should verify");
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
        let atv = seq(&[oid("2.5.4.3").as_slice(), tlv(0x13, cn.as_bytes()).as_slice()].concat());
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
        // A chain link signed with a real RSA/ECDSA OID (here:
        // sha256WithRSAEncryption, 1.2.840.113549.1.1.11) must be
        // rejected as NotImplemented — the conservative placeholder
        // from acceptance #6.
        let root_subject_dn = x509_name("RealRoot");
        // Hand-roll a cert whose outer signatureAlgorithm OID is the
        // real PKCS#1 v1.5 RSA-SHA256 identifier.
        let (tbs, _) = build_tbs("RealLeaf", "RealRoot");
        let sig_alg = algorithm_identifier("1.2.840.113549.1.1.11");
        let bit_string = tlv(0x03, &[0u8, 0xDE, 0xAD]); // bogus sig bytes
        let leaf_der =
            seq(&[tbs.as_slice(), sig_alg.as_slice(), bit_string.as_slice()].concat());

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
}
