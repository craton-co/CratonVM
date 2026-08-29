// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.6 — minimal ASN.1 DER encoder/decoder for the JCA surface.
//!
//! The only DER-touching probe in the wave is the X500Principal round-trip
//! (`apps/sig_probe/SigProbe.java`):
//!
//! ```java
//! X500Principal dn = new X500Principal("CN=Test, O=Acme, C=SE");
//! byte[] der = dn.getEncoded();
//! X500Principal back = new X500Principal(der);
//! if (!dn.equals(back)) { ... }
//! ```
//!
//! The codec lives here so future Wave-6 surfaces (KeyFactory, Signature
//! parameter encoding) reuse the same byte-correct primitives.  Existing
//! `crypto_impl::der_encode_*` helpers cover what we need for INTEGER /
//! SEQUENCE / OCTET STRING; this module adds OID, SET, and the typed-string
//! encodings (`PrintableString`, `UTF8String`, `IA5String`) the X.500 RDN
//! values use.
//!
//! Hard size limits: 1 MiB total input, 64 levels of nesting (matches the
//! `security_manager::x509` parser already shipped in session 90).
//!
//! ## WP8.11.6 — PKCS#10 / X.509 v3 helpers
//!
//! BC's `JcaPKCS10CertificationRequestBuilder.build(signer)` (used by
//! EJBCA's first cert-issue call) materialises three new SEQUENCE shapes
//! we did not previously encode:
//!
//! ```text
//! AlgorithmIdentifier ::= SEQUENCE {
//!     algorithm   OBJECT IDENTIFIER,
//!     parameters  ANY DEFINED BY algorithm OPTIONAL
//! }
//!
//! SubjectPublicKeyInfo ::= SEQUENCE {
//!     algorithm        AlgorithmIdentifier,
//!     subjectPublicKey BIT STRING
//! }
//!
//! Extensions ::= SEQUENCE SIZE (1..MAX) OF Extension
//! Extension   ::= SEQUENCE {
//!     extnID    OBJECT IDENTIFIER,
//!     critical  BOOLEAN DEFAULT FALSE,
//!     extnValue OCTET STRING
//! }
//! ```
//!
//! And the wrapping `CertificationRequestInfo` (RFC 2986 §4.1) and a
//! lightweight `TBSCertificate` skeleton (RFC 5280 §4.1).  The encoders
//! below are deliberately byte-faithful: BC's `DERSequence.encode` emits
//! tag, length, then content with no extra padding, the same shape we
//! already produce — the test fixtures cross-check that property.

#![allow(dead_code)]

const MAX_INPUT_SIZE: usize = 1024 * 1024;
const MAX_NESTING_DEPTH: usize = 64;

// DER tag bytes
pub const TAG_BOOLEAN: u8 = 0x01;
pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_UTF8_STRING: u8 = 0x0C;
pub const TAG_PRINTABLE: u8 = 0x13;
pub const TAG_TELETEX: u8 = 0x14;
pub const TAG_IA5: u8 = 0x16;
pub const TAG_BMP: u8 = 0x1E;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;

// ---------------------------------------------------------------------------
// Encoder helpers
// ---------------------------------------------------------------------------

/// Encode a length following X.690 §8.1.3 (definite-length only, DER).
pub fn encode_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x100 {
        vec![0x81, len as u8]
    } else if len < 0x10000 {
        vec![0x82, (len >> 8) as u8, len as u8]
    } else if len < 0x1000000 {
        vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
    } else {
        // 4-byte length (caps at 4 GiB; X.500 names never go this big).
        vec![
            0x84,
            (len >> 24) as u8,
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
        ]
    }
}

/// `tag || len || content`.
pub fn encode_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    out.extend_from_slice(&encode_length(content.len()));
    out.extend_from_slice(content);
    out
}

/// `SEQUENCE { content }`.
pub fn encode_sequence(content: &[u8]) -> Vec<u8> {
    encode_tlv(TAG_SEQUENCE, content)
}

/// `SET { content }`.  Per DER (X.690 §10.3) elements of a `SET OF` must
/// be sorted by their encoded byte value; we produce single-element sets
/// from RDNs so sorting is a no-op for the X500Principal round-trip.
pub fn encode_set(content: &[u8]) -> Vec<u8> {
    encode_tlv(TAG_SET, content)
}

/// Encode a dotted-decimal OID like "2.5.4.3" into DER bytes.
///
/// First byte = `arc1 * 40 + arc2`.  Subsequent arcs encoded base-128
/// big-endian with the high bit of the final byte cleared.  Returns
/// `Ok(_)` on success, `Err(())` on malformed input (so the caller can
/// fall back to a synthetic encoding without panicking).
pub fn encode_oid(dotted: &str) -> Result<Vec<u8>, ()> {
    let arcs: Result<Vec<u64>, _> = dotted.split('.').map(|s| s.parse::<u64>()).collect();
    let arcs = arcs.map_err(|_| ())?;
    if arcs.len() < 2 {
        return Err(());
    }
    if arcs[0] > 2 || arcs[1] > 39 {
        return Err(());
    }
    let mut content = Vec::new();
    content.push((arcs[0] * 40 + arcs[1]) as u8);
    for &arc in &arcs[2..] {
        encode_base128(&mut content, arc);
    }
    Ok(encode_tlv(TAG_OID, &content))
}

fn encode_base128(out: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        out.push(0);
        return;
    }
    // Collect base-128 digits least-significant first.
    let mut digits = Vec::new();
    while v > 0 {
        digits.push((v & 0x7F) as u8);
        v >>= 7;
    }
    // Emit most-significant first; set high bit on all but last.
    for (i, d) in digits.iter().rev().enumerate() {
        let last = i + 1 == digits.len();
        out.push(if last { *d } else { *d | 0x80 });
    }
}

/// Pick an RDN string-tag based on the value's content.
///
/// JDK's X500Name encoder uses `PrintableString` when every character is in
/// the ASCII printable subset (A-Z, a-z, 0-9, plus ` '()+,-./:=?`) and
/// otherwise falls back to `UTF8String`.  IA5String is reserved for `EmailAddress`
/// and `DC` per RFC 5280 §4.1.2.4.
pub fn pick_string_tag(value: &str) -> u8 {
    if value.bytes().all(|b| {
        matches!(b,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
            b' ' | b'\'' | b'(' | b')' | b'+' | b',' | b'-' |
            b'.' | b'/' | b':' | b'=' | b'?'
        )
    }) {
        TAG_PRINTABLE
    } else {
        TAG_UTF8_STRING
    }
}

/// Encode a DirectoryString — chooses between PrintableString / UTF8String.
pub fn encode_directory_string(value: &str) -> Vec<u8> {
    encode_tlv(pick_string_tag(value), value.as_bytes())
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerError {
    Truncated,
    BadTag,
    BadLength,
    TooLarge,
    NestedTooDeep,
    NotMinimal,
    Trailing,
}

/// `(tag, content_offset, content_len, total_len)` — total_len includes the
/// tag and length bytes.
pub fn read_header(data: &[u8]) -> Result<(u8, usize, usize, usize), DerError> {
    if data.is_empty() {
        return Err(DerError::Truncated);
    }
    let tag = data[0];
    if data.len() < 2 {
        return Err(DerError::Truncated);
    }
    let l0 = data[1];
    let (content_len, hdr_extra) = if l0 < 0x80 {
        (l0 as usize, 1)
    } else if l0 == 0x80 {
        // Indefinite length — not allowed in DER.
        return Err(DerError::BadLength);
    } else {
        let n = (l0 & 0x7F) as usize;
        if n > 4 {
            return Err(DerError::BadLength);
        }
        if data.len() < 2 + n {
            return Err(DerError::Truncated);
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | (data[2 + i] as usize);
        }
        (len, 1 + n)
    };
    let hdr_total = 1 + hdr_extra;
    let total = hdr_total
        .checked_add(content_len)
        .ok_or(DerError::TooLarge)?;
    if total > MAX_INPUT_SIZE || data.len() < total {
        return Err(DerError::Truncated);
    }
    Ok((tag, hdr_total, content_len, total))
}

/// Read an OID into its dotted-decimal string form.
pub fn read_oid(content: &[u8]) -> Result<String, DerError> {
    if content.is_empty() {
        return Err(DerError::BadLength);
    }
    let first = content[0] as u64;
    let arc1 = first / 40;
    let arc2 = first % 40;
    let mut out = format!("{}.{}", arc1, arc2);
    let mut i = 1;
    while i < content.len() {
        let mut v: u64 = 0;
        loop {
            if i >= content.len() {
                return Err(DerError::Truncated);
            }
            let b = content[i];
            i += 1;
            // Reject arcs that would overflow u64: if any of the top 7 bits of
            // the accumulator are set, `v << 7` would discard them and yield a
            // silently-wrong dotted-decimal value (and panics in debug builds).
            if (v >> 57) != 0 {
                return Err(DerError::TooLarge);
            }
            v = (v << 7) | ((b & 0x7F) as u64);
            if (b & 0x80) == 0 {
                break;
            }
        }
        out.push('.');
        out.push_str(&v.to_string());
    }
    Ok(out)
}

/// Decode a directory string (PrintableString / UTF8String / IA5String /
/// TeletexString / BMPString) into a Rust `String`.
pub fn read_directory_string(tag: u8, content: &[u8]) -> Option<String> {
    match tag {
        TAG_PRINTABLE | TAG_UTF8_STRING | TAG_IA5 | TAG_TELETEX => {
            // Treat all of these as UTF-8 (Latin-1 fallback for Teletex).
            match std::str::from_utf8(content) {
                Ok(s) => Some(s.to_string()),
                Err(_) => Some(content.iter().map(|&b| b as char).collect::<String>()),
            }
        }
        TAG_BMP => {
            // BMPString is UCS-2 big-endian.
            if content.len() % 2 != 0 {
                return None;
            }
            let mut out = String::with_capacity(content.len() / 2);
            for chunk in content.chunks_exact(2) {
                let cp = u16::from_be_bytes([chunk[0], chunk[1]]);
                out.push(char::from_u32(cp as u32).unwrap_or('?'));
            }
            Some(out)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Generic encoders (INTEGER, BIT STRING, OCTET STRING, BOOLEAN, NULL, [n])
// ---------------------------------------------------------------------------

/// Encode an unsigned big-integer as a DER `INTEGER`.
///
/// DER requires the encoding to be the shortest two's-complement form, so
/// when the high bit of the first byte is set we prepend a `0x00` to keep
/// the value non-negative.  Empty inputs encode as `INTEGER 0`.
pub fn encode_integer_unsigned(bytes: &[u8]) -> Vec<u8> {
    // Trim leading zero bytes (X.690 §8.3.2 minimal encoding).
    let mut start = 0;
    while start + 1 < bytes.len() && bytes[start] == 0 {
        start += 1;
    }
    let trimmed = &bytes[start..];
    if trimmed.is_empty() {
        return encode_tlv(TAG_INTEGER, &[0]);
    }
    if trimmed[0] & 0x80 != 0 {
        // Avoid two's-complement sign flip.
        let mut content = Vec::with_capacity(trimmed.len() + 1);
        content.push(0x00);
        content.extend_from_slice(trimmed);
        encode_tlv(TAG_INTEGER, &content)
    } else {
        encode_tlv(TAG_INTEGER, trimmed)
    }
}

/// Encode a small non-negative `INTEGER` in canonical DER.  The most-
/// common use is the `version` field of `CertificationRequestInfo` /
/// `TBSCertificate`.
pub fn encode_integer_u64(value: u64) -> Vec<u8> {
    if value == 0 {
        return encode_tlv(TAG_INTEGER, &[0]);
    }
    let mut bytes = Vec::with_capacity(8);
    let mut v = value;
    while v > 0 {
        bytes.push((v & 0xFF) as u8);
        v >>= 8;
    }
    bytes.reverse();
    if bytes[0] & 0x80 != 0 {
        let mut content = Vec::with_capacity(bytes.len() + 1);
        content.push(0x00);
        content.extend_from_slice(&bytes);
        encode_tlv(TAG_INTEGER, &content)
    } else {
        encode_tlv(TAG_INTEGER, &bytes)
    }
}

/// Encode a primitive `OCTET STRING`.
pub fn encode_octet_string(content: &[u8]) -> Vec<u8> {
    encode_tlv(TAG_OCTET_STRING, content)
}

/// Encode a primitive `BIT STRING` carrying *whole* octets (no unused
/// bits).  This is the shape `SubjectPublicKeyInfo.subjectPublicKey`
/// uses — the unused-bits count is always 0 because the wrapped DER blob
/// is byte-aligned.
pub fn encode_bit_string_aligned(payload: &[u8]) -> Vec<u8> {
    let mut content = Vec::with_capacity(payload.len() + 1);
    content.push(0x00); // unused bits
    content.extend_from_slice(payload);
    encode_tlv(TAG_BIT_STRING, &content)
}

/// Encode a `BOOLEAN`.  DER §11.1 — TRUE *must* be encoded as `0xFF`.
pub fn encode_boolean(value: bool) -> Vec<u8> {
    encode_tlv(TAG_BOOLEAN, &[if value { 0xFF } else { 0x00 }])
}

/// Encode an explicit context-specific tag `[n]` wrapping `inner`.  This
/// is what `CertificationRequestInfo.attributes [0]` and TBSCertificate's
/// `[3] EXPLICIT extensions` need.  The constructed bit (0x20) is set so
/// the resulting tag is `0xA0 | n`.
pub fn encode_explicit_context(tag_number: u8, inner: &[u8]) -> Vec<u8> {
    debug_assert!(tag_number < 0x1F, "high-tag-number form not supported");
    let tag = 0xA0 | (tag_number & 0x1F);
    encode_tlv(tag, inner)
}

/// Encode a `NULL` primitive — `05 00`.  Used as the `parameters` of
/// `AlgorithmIdentifier` for algorithms like `sha256WithRSAEncryption`
/// where RFC 4055 §2.1 says parameters MUST be present and NULL.
pub fn encode_null() -> Vec<u8> {
    vec![TAG_NULL, 0x00]
}

// ---------------------------------------------------------------------------
// AlgorithmIdentifier (RFC 5280 §4.1.1.2)
// ---------------------------------------------------------------------------

/// `AlgorithmIdentifier ::= SEQUENCE { algorithm OID, parameters ANY OPTIONAL }`.
///
/// `params_der` is treated as opaque pre-encoded DER.  Pass `None` to
/// omit parameters entirely (used by EC algorithms where parameters are
/// either absent or carry a named-curve OID), `Some(&encode_null())` for
/// the RSA-style `NULL` parameters, or `Some(&pre_encoded_oid)` for EC
/// named-curve identifiers.
pub fn encode_algorithm_identifier(oid_dotted: &str, params_der: Option<&[u8]>) -> Vec<u8> {
    let oid_der =
        encode_oid(oid_dotted).unwrap_or_else(|_| encode_tlv(TAG_OID, oid_dotted.as_bytes()));
    let mut inner = Vec::with_capacity(oid_der.len() + params_der.map_or(0, |p| p.len()));
    inner.extend_from_slice(&oid_der);
    if let Some(p) = params_der {
        inner.extend_from_slice(p);
    }
    encode_sequence(&inner)
}

/// Decode an `AlgorithmIdentifier` into `(oid_dotted, params_der_opt)`.
///
/// `params_der_opt` is the raw TLV bytes of the optional parameters
/// element (NULL, OID, or any other ANY-DEFINED-BY value), preserved for
/// re-encoding.  `None` means parameters were absent.
pub fn decode_algorithm_identifier(der: &[u8]) -> Result<(String, Option<Vec<u8>>), DerError> {
    let (tag, hdr, content_len, _total) = read_header(der)?;
    if tag != TAG_SEQUENCE {
        return Err(DerError::BadTag);
    }
    let content = &der[hdr..hdr + content_len];
    // OID
    let (otag, ohdr, oclen, ototal) = read_header(content)?;
    if otag != TAG_OID {
        return Err(DerError::BadTag);
    }
    let oid = read_oid(&content[ohdr..ohdr + oclen])?;
    // Optional parameters (whatever's left).
    let params = if ototal < content.len() {
        // Validate the trailing element is well-formed before keeping it.
        let (_, _, _, ptotal) = read_header(&content[ototal..])?;
        if ototal + ptotal != content.len() {
            return Err(DerError::Trailing);
        }
        Some(content[ototal..ototal + ptotal].to_vec())
    } else {
        None
    };
    Ok((oid, params))
}

// ---------------------------------------------------------------------------
// SubjectPublicKeyInfo (RFC 5280 §4.1.2.7)
// ---------------------------------------------------------------------------

/// `SubjectPublicKeyInfo ::= SEQUENCE { algorithm AlgorithmIdentifier,
///  subjectPublicKey BIT STRING }`.
///
/// `algorithm_oid` + `algorithm_params` flow through to
/// `encode_algorithm_identifier`.  `key_bits` is the raw public-key
/// bytes (e.g. RSA's PKCS#1 `RSAPublicKey` SEQUENCE, or EC's uncompressed
/// point).  We always emit unused-bits = 0 because every public-key
/// encoding we care about is byte-aligned.
pub fn encode_subject_public_key_info(
    algorithm_oid: &str,
    algorithm_params: Option<&[u8]>,
    key_bits: &[u8],
) -> Vec<u8> {
    let alg_id = encode_algorithm_identifier(algorithm_oid, algorithm_params);
    let bit_str = encode_bit_string_aligned(key_bits);
    let mut inner = Vec::with_capacity(alg_id.len() + bit_str.len());
    inner.extend_from_slice(&alg_id);
    inner.extend_from_slice(&bit_str);
    encode_sequence(&inner)
}

/// Decoded view of a SubjectPublicKeyInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectPublicKeyInfo {
    pub algorithm_oid: String,
    pub algorithm_params: Option<Vec<u8>>,
    /// Raw public-key bytes (the BIT STRING content with the unused-bits
    /// prefix stripped).
    pub subject_public_key: Vec<u8>,
}

/// Decode a SubjectPublicKeyInfo SEQUENCE.
pub fn decode_subject_public_key_info(der: &[u8]) -> Result<SubjectPublicKeyInfo, DerError> {
    let (tag, hdr, content_len, _) = read_header(der)?;
    if tag != TAG_SEQUENCE {
        return Err(DerError::BadTag);
    }
    let content = &der[hdr..hdr + content_len];
    // AlgorithmIdentifier
    let (atag, _ahdr, _aclen, atot) = read_header(content)?;
    if atag != TAG_SEQUENCE {
        return Err(DerError::BadTag);
    }
    let alg_der = &content[..atot];
    let (algorithm_oid, algorithm_params) = decode_algorithm_identifier(alg_der)?;
    // BIT STRING
    let (btag, bhdr, bclen, _) = read_header(&content[atot..])?;
    if btag != TAG_BIT_STRING {
        return Err(DerError::BadTag);
    }
    let bit_content = &content[atot + bhdr..atot + bhdr + bclen];
    if bit_content.is_empty() {
        return Err(DerError::BadLength);
    }
    let unused = bit_content[0];
    if unused != 0 {
        // We only encode aligned BIT STRINGs; non-zero unused-bits is a
        // signal that this SPKI was produced by some other tool.  Keep
        // the bytes anyway — callers can interpret them.
    }
    Ok(SubjectPublicKeyInfo {
        algorithm_oid,
        algorithm_params,
        subject_public_key: bit_content[1..].to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Extensions (RFC 5280 §4.1.2.9 / §4.2)
// ---------------------------------------------------------------------------

/// One Extension before encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub oid: String,
    pub critical: bool,
    /// Pre-encoded DER for the extension's payload.  RFC 5280 wraps it
    /// in an OCTET STRING; the encoder handles that.
    pub value: Vec<u8>,
}

/// Encode a single `Extension` SEQUENCE.
///
/// The `critical` BOOLEAN is omitted when false (DER §11.5 — DEFAULT
/// values must be absent).  This matches BC's
/// `org.bouncycastle.asn1.x509.Extension.toASN1Primitive` exactly — the
/// EJBCA → BC interop check drives this rule.
pub fn encode_extension(ext: &Extension) -> Vec<u8> {
    let oid_der = encode_oid(&ext.oid).unwrap_or_else(|_| encode_tlv(TAG_OID, ext.oid.as_bytes()));
    let crit_der = if ext.critical {
        Some(encode_boolean(true))
    } else {
        None
    };
    let value_der = encode_octet_string(&ext.value);
    let mut inner = Vec::with_capacity(
        oid_der.len() + crit_der.as_ref().map_or(0, |c| c.len()) + value_der.len(),
    );
    inner.extend_from_slice(&oid_der);
    if let Some(c) = &crit_der {
        inner.extend_from_slice(c);
    }
    inner.extend_from_slice(&value_der);
    encode_sequence(&inner)
}

/// Encode a top-level `Extensions ::= SEQUENCE OF Extension`.
pub fn encode_extensions(exts: &[Extension]) -> Vec<u8> {
    let mut inner = Vec::new();
    for e in exts {
        inner.extend_from_slice(&encode_extension(e));
    }
    encode_sequence(&inner)
}

/// Decode a single Extension SEQUENCE into the typed struct.
pub fn decode_extension(der: &[u8]) -> Result<Extension, DerError> {
    let (tag, hdr, content_len, _) = read_header(der)?;
    if tag != TAG_SEQUENCE {
        return Err(DerError::BadTag);
    }
    let content = &der[hdr..hdr + content_len];
    // OID
    let (otag, ohdr, oclen, ototal) = read_header(content)?;
    if otag != TAG_OID {
        return Err(DerError::BadTag);
    }
    let oid = read_oid(&content[ohdr..ohdr + oclen])?;
    let mut pos = ototal;
    // Optional BOOLEAN
    let mut critical = false;
    let (next_tag, next_hdr, next_clen, next_total) = read_header(&content[pos..])?;
    if next_tag == TAG_BOOLEAN {
        if next_clen != 1 {
            return Err(DerError::BadLength);
        }
        critical = content[pos + next_hdr] != 0x00;
        pos += next_total;
    }
    // OCTET STRING
    let (vtag, vhdr, vclen, _) = read_header(&content[pos..])?;
    if vtag != TAG_OCTET_STRING {
        return Err(DerError::BadTag);
    }
    let value = content[pos + vhdr..pos + vhdr + vclen].to_vec();
    Ok(Extension {
        oid,
        critical,
        value,
    })
}

/// Decode a top-level `Extensions` SEQUENCE.
pub fn decode_extensions(der: &[u8]) -> Result<Vec<Extension>, DerError> {
    let (tag, hdr, content_len, _) = read_header(der)?;
    if tag != TAG_SEQUENCE {
        return Err(DerError::BadTag);
    }
    let content = &der[hdr..hdr + content_len];
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < content.len() {
        let (_, _, _, total) = read_header(&content[pos..])?;
        out.push(decode_extension(&content[pos..pos + total])?);
        pos += total;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CertificationRequestInfo (RFC 2986 §4.1)
// ---------------------------------------------------------------------------

/// CertificationRequestInfo ::= SEQUENCE {
///     version       INTEGER { v1(0) },
///     subject       Name,
///     subjectPKInfo SubjectPublicKeyInfo,
///     attributes    \[0\] Attributes
/// }
///
/// `subject_name_der` is the *complete* DER blob produced by
/// `x500::encode_rdns_to_der` (i.e. an X.500 `Name` SEQUENCE).
/// `subject_pki_der` is the SPKI SEQUENCE.  `attributes_der` is the
/// inner SEQUENCE OF Attribute *content*; we wrap it in `[0]` here.
/// Pass an empty slice to emit `[0] {}` (BC does this for plain CSRs
/// with no extension request).
pub fn encode_certification_request_info(
    version: u64,
    subject_name_der: &[u8],
    subject_pki_der: &[u8],
    attributes_inner: &[u8],
) -> Vec<u8> {
    let mut inner = Vec::with_capacity(
        16 + subject_name_der.len() + subject_pki_der.len() + attributes_inner.len(),
    );
    inner.extend_from_slice(&encode_integer_u64(version));
    inner.extend_from_slice(subject_name_der);
    inner.extend_from_slice(subject_pki_der);
    inner.extend_from_slice(&encode_explicit_context(0, attributes_inner));
    encode_sequence(&inner)
}

// ---------------------------------------------------------------------------
// TBSCertificate skeleton (RFC 5280 §4.1)
// ---------------------------------------------------------------------------

/// Minimal TBSCertificate encoder for X.509 v3.
///
/// `TBSCertificate ::= SEQUENCE {
///     version         \[0\] EXPLICIT INTEGER DEFAULT v1,
///     serialNumber    INTEGER,
///     signature       AlgorithmIdentifier,
///     issuer          Name,
///     validity        SEQUENCE { notBefore Time, notAfter Time },
///     subject         Name,
///     subjectPublicKeyInfo SubjectPublicKeyInfo,
///     ...
///     extensions      \[3\] EXPLICIT Extensions OPTIONAL
/// }`
///
/// All inputs except `serial` and `extensions` are pre-encoded DER
/// blobs.  `version_v3` controls whether to emit the explicit `[0] 2`
/// version tag.  `validity_der` is the pre-built validity SEQUENCE
/// (callers compose this from two `UTCTime` / `GeneralizedTime` TLVs —
/// out of scope for this helper).  `extensions_der` is the wrapped
/// Extensions SEQUENCE; the helper adds the `[3] EXPLICIT` wrapper.
///
/// This is a *skeleton* — Wave-6 + WP8.11.6 only need it to round-trip
/// the BC test fixtures; full v3 cert issuance is deferred to whatever
/// future WP wires up `X509CertificateGenerator`.
pub fn encode_tbs_certificate(
    version_v3: bool,
    serial: &[u8],
    signature_alg_der: &[u8],
    issuer_name_der: &[u8],
    validity_der: &[u8],
    subject_name_der: &[u8],
    subject_pki_der: &[u8],
    extensions_der: Option<&[u8]>,
) -> Vec<u8> {
    let mut inner = Vec::new();
    if version_v3 {
        // [0] EXPLICIT INTEGER 2.
        let v = encode_integer_u64(2);
        inner.extend_from_slice(&encode_explicit_context(0, &v));
    }
    inner.extend_from_slice(&encode_integer_unsigned(serial));
    inner.extend_from_slice(signature_alg_der);
    inner.extend_from_slice(issuer_name_der);
    inner.extend_from_slice(validity_der);
    inner.extend_from_slice(subject_name_der);
    inner.extend_from_slice(subject_pki_der);
    if let Some(ext) = extensions_der {
        inner.extend_from_slice(&encode_explicit_context(3, ext));
    }
    encode_sequence(&inner)
}

// ---------------------------------------------------------------------------
// Common signature/key OIDs (RFC 5280 §A.2 + RFC 8017 §A.2)
// ---------------------------------------------------------------------------

/// `rsaEncryption` — the algorithm OID for RSA SubjectPublicKeyInfo.
pub const OID_RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
/// `sha256WithRSAEncryption`.
pub const OID_SHA256_WITH_RSA: &str = "1.2.840.113549.1.1.11";
/// `sha384WithRSAEncryption`.
pub const OID_SHA384_WITH_RSA: &str = "1.2.840.113549.1.1.12";
/// `sha512WithRSAEncryption`.
pub const OID_SHA512_WITH_RSA: &str = "1.2.840.113549.1.1.13";
/// `id-ecPublicKey`.
pub const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
/// `ecdsa-with-SHA256`.
pub const OID_ECDSA_WITH_SHA256: &str = "1.2.840.10045.4.3.2";
/// `secp256r1` / P-256 named curve.
pub const OID_SECP256R1: &str = "1.2.840.10045.3.1.7";
/// `id-ce-extKeyUsage`.
pub const OID_EXT_KEY_USAGE: &str = "2.5.29.37";
/// `id-ce-basicConstraints`.
pub const OID_BASIC_CONSTRAINTS: &str = "2.5.29.19";

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn encode_length_short_form() {
        assert_eq!(encode_length(0), vec![0x00]);
        assert_eq!(encode_length(127), vec![0x7F]);
    }

    #[test]
    fn encode_length_long_form() {
        assert_eq!(encode_length(128), vec![0x81, 0x80]);
        assert_eq!(encode_length(0x100), vec![0x82, 0x01, 0x00]);
        assert_eq!(encode_length(0x10000), vec![0x83, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn oid_round_trip_2_5_4_3() {
        // 2.5.4.3 -> 0x55 0x04 0x03
        let der = encode_oid("2.5.4.3").unwrap();
        assert_eq!(der, vec![0x06, 0x03, 0x55, 0x04, 0x03]);
        let (tag, hdr, content_len, total) = read_header(&der).unwrap();
        assert_eq!(tag, TAG_OID);
        assert_eq!(content_len, 3);
        assert_eq!(total, 5);
        let oid = read_oid(&der[hdr..hdr + content_len]).unwrap();
        assert_eq!(oid, "2.5.4.3");
    }

    #[test]
    fn oid_round_trip_long_arc() {
        // 1.2.840.113549.1.1.1 (rsaEncryption)
        let der = encode_oid("1.2.840.113549.1.1.1").unwrap();
        let (tag, hdr, content_len, _) = read_header(&der).unwrap();
        assert_eq!(tag, TAG_OID);
        let back = read_oid(&der[hdr..hdr + content_len]).unwrap();
        assert_eq!(back, "1.2.840.113549.1.1.1");
    }

    #[test]
    fn pick_string_tag_basic() {
        assert_eq!(pick_string_tag("Test"), TAG_PRINTABLE);
        assert_eq!(pick_string_tag("Acme Corp"), TAG_PRINTABLE);
        // Non-printable: contains '!'
        assert_eq!(pick_string_tag("hi!"), TAG_UTF8_STRING);
        // UTF-8 chars
        assert_eq!(pick_string_tag("Acmé"), TAG_UTF8_STRING);
    }

    #[test]
    fn read_header_rejects_indefinite() {
        let data = vec![0x30, 0x80];
        assert_eq!(read_header(&data), Err(DerError::BadLength));
    }

    #[test]
    fn directory_string_utf8_and_bmp() {
        assert_eq!(
            read_directory_string(TAG_UTF8_STRING, b"hello").as_deref(),
            Some("hello")
        );
        // BMP UCS-2BE for "hi"
        assert_eq!(
            read_directory_string(TAG_BMP, &[0x00, b'h', 0x00, b'i']).as_deref(),
            Some("hi")
        );
    }
}
