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
//! * **Trust-store integration.**  We do not yet check that the leaf
//!   cert chains to a trusted root — every well-formed self-signed JAR
//!   currently passes self-consistency.  Wiring trust-store probes
//!   belongs in a follow-up that owns the keystore-loading path.
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
/// self-consistent with the matching `*.SF` bytes.
///
/// `signer_block_der` is the raw signer-block file contents.  `sf_bytes`
/// is the corresponding signature-file (`*.SF`) contents — typically
/// found by stripping the extension and looking up `META-INF/{stem}.SF`
/// in the same JAR.
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
///     field.
///
/// Returns `None` on *any* failure (parse error, truncation, OID
/// mismatch, digest mismatch, missing cert, ...).  Logs at
/// `tracing::warn` for diagnostics.  **Never panics.**
///
/// # Security limits
///
/// `signer_block_der.len()` is hard-capped at 1 MiB; larger inputs are
/// rejected immediately.  Cert DER blobs >64 KiB are dropped from the
/// returned chain (defense against single-cert zip-bombs).
///
/// # TODO(post-orchestrator)
///
/// Pub-key signature verification over the authenticated-attributes
/// blob is not yet implemented — see module-level docs.  Trust-store
/// chaining is likewise deferred.
pub fn verify_signer_block(
    signer_block_der: &[u8],
    sf_bytes: &[u8],
) -> Option<VerifiedSigner> {
    if signer_block_der.is_empty() || signer_block_der.len() > MAX_SIGNER_BLOCK {
        warn!(
            "jar signer: rejecting signer block of size {} (limit {})",
            signer_block_der.len(),
            MAX_SIGNER_BLOCK
        );
        return None;
    }
    match parse_signed_data(signer_block_der, sf_bytes) {
        Ok(vs) => Some(vs),
        Err(e) => {
            warn!("jar signer: rejecting signer block: {}", e);
            None
        }
    }
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
    let mut sd = Cursor::new(signed_data_seq);

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
    let signer_infos = split_set_or_seq(signer_infos, MAX_DEPTH)?;
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
    let mut si = Cursor::new(signer_info);
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
        let mut a = Cursor::new(attr_seq);
        let attr_oid = a.read_oid()?;
        let attr_values = a.read_set()?;
        match attr_oid.as_str() {
            OID_CONTENT_TYPE => {
                // SET OF OID containing pkcs7-data.
                let mut v = Cursor::new(attr_values);
                let inner = v.read_oid()?;
                if inner != OID_DATA {
                    return Err("contentType attribute is not pkcs7-data");
                }
                saw_content_type = true;
            }
            OID_MESSAGE_DIGEST => {
                // SET OF OCTET STRING.
                let mut v = Cursor::new(attr_values);
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
            let mut a = Cursor::new(atv);
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
        let vs = verify_signer_block(&block, sf).expect("well-formed block should verify");
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
        let vs = verify_signer_block(&block, sf).expect("SHA-1 block should verify");
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
            verify_signer_block(&block, sf_tampered).is_none(),
            "tampered .SF must be rejected"
        );

        // Sanity: original .SF still verifies.
        assert!(verify_signer_block(&block, sf_original).is_some());
    }

    #[test]
    fn rejects_tampered_message_digest_attribute() {
        // The block itself has been tampered to flip the messageDigest
        // attribute (without recomputing the signature).  Even though
        // the .SF is genuine, the recomputed SHA-256 won't equal the
        // stored bytes.
        let sf = b"Signature-Version: 1.0\r\n\r\n";
        let block = build_signer_block(sf, DigestAlg::Sha256, /*tamper=*/ true);
        assert!(verify_signer_block(&block, sf).is_none());
    }

    #[test]
    fn rejects_empty_block_without_panic() {
        assert!(verify_signer_block(&[], b"sf").is_none());
    }

    #[test]
    fn rejects_truncated_block_without_panic() {
        // A 3-byte stub — not even enough for a TLV header / length.
        assert!(verify_signer_block(&[0x30, 0x82, 0xFF], b"sf").is_none());
    }

    #[test]
    fn rejects_garbage_bytes_without_panic() {
        // 256 random-ish bytes that aren't valid DER.
        let garbage: Vec<u8> = (0..=255u8).collect();
        assert!(verify_signer_block(&garbage, b"sf").is_none());
    }

    #[test]
    fn rejects_oversized_block_without_panic() {
        let huge = vec![0u8; MAX_SIGNER_BLOCK + 1];
        assert!(verify_signer_block(&huge, b"sf").is_none());
    }

    #[test]
    fn rejects_block_with_wrong_outer_oid() {
        // Replace pkcs7-signedData with pkcs7-data — should bounce.
        let inner = seq(&oid("1.2.3.4.5"));
        let bad = seq(&[oid(OID_DATA).as_slice(), ctx_imp(0, &inner).as_slice()].concat());
        assert!(verify_signer_block(&bad, b"sf").is_none());
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
        assert!(verify_signer_block(&block, b"sf").is_none());
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
        assert!(verify_signer_block(&block, b"sf").is_none());
    }

    #[test]
    fn rejects_unsupported_digest_alg() {
        // SHA-512 — recognised OID but the implementation returns an
        // empty digest, which can never equal the stored 64-byte digest.
        let sf = b"any";
        let block = build_signer_block(sf, DigestAlg::Sha512, /*tamper=*/ false);
        assert!(verify_signer_block(&block, sf).is_none());
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
}
