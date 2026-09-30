// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Hand-rolled X.509 / PKCS#7 Subject-DN parser for the SecurityManager
//! policy engine.
//!
//! Policy files can carry `grant signedBy "CN=Acme Corp"` clauses.  At
//! class-load time we already have the raw PKCS#7 / CMS SignedData bytes
//! for each JAR signer stashed on the class's `CodeSource`.  This module
//! turns those bytes into a canonical RFC 4514 Distinguished-Name string
//! so the policy engine can substring-match the alias without pulling in
//! a heavyweight ASN.1 crate.
//!
//! Supported input: PKCS#7 ContentInfo → SignedData → certificates → first
//! X.509 certificate → tbsCertificate.subject as a RDNSequence of RDNs,
//! where each RDN is a SET of `AttributeTypeAndValue` pairs. Attribute
//! values we recognise are `PrintableString`, `UTF8String`, `IA5String`,
//! `TeletexString` and `BMPString` (the last two are decoded as Latin-1
//! / UCS-2 respectively).  Everything else falls back to a hex
//! placeholder so the parser never fails loudly on an exotic value type.
//!
//! Output: a string like `"CN=Acme Corp, OU=Eng, O=Acme, C=US"`.  Per
//! RFC 4514, RDNs are emitted in reverse order (most-specific first) and
//! the characters `"+,;<>\\"`, leading `#`, leading/trailing space are
//! backslash-escaped.  If the Name contains a multi-valued RDN (rare),
//! the attributes inside are joined with `'+'` in stable order.
//!
//! Error handling: every decode step returns `Result<_, X509Error>`.
//! Oversized blobs (> 1 MiB) and pathologically-nested ASN.1 (> 64
//! levels) are rejected up front so a maliciously-crafted JAR signer
//! cannot turn parser failure into a DoS.  No `unwrap` / `expect` on
//! anything derived from the input.

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

/// Reasons a PKCS#7 blob may fail to yield a signer DN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X509Error {
    /// The outer ContentInfo is missing the SignedData OID
    /// (1.2.840.113549.1.7.2) or the tag layout doesn't match CMS.
    NotPkcs7,
    /// ASN.1 length, tag, or wrapper is malformed / truncated.
    MalformedAsn1,
    /// PKCS#7 SignedData contains no `certificates [0] IMPLICIT` field
    /// or the field is empty.
    NoSignerCert,
    /// The certificates[0] entry does not parse as an X.509 Certificate.
    MalformedCert,
    /// The Certificate's tbsCertificate has no Subject RDNSequence, or
    /// the RDNSequence contains no RDNs.
    NoSubject,
    /// Input exceeds the 1 MiB size limit or nesting exceeds 64 levels.
    TooLarge,
}

impl std::fmt::Display for X509Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            X509Error::NotPkcs7 => write!(f, "input is not a PKCS#7 SignedData blob"),
            X509Error::MalformedAsn1 => write!(f, "malformed ASN.1 encoding"),
            X509Error::NoSignerCert => write!(f, "PKCS#7 has no signer certificate"),
            X509Error::MalformedCert => write!(f, "signer X.509 certificate is malformed"),
            X509Error::NoSubject => write!(f, "signer certificate has no Subject"),
            X509Error::TooLarge => write!(f, "input is too large or too deeply nested"),
        }
    }
}

impl std::error::Error for X509Error {}

// ---------------------------------------------------------------------------
// Size / depth limits.  The DoS surface is bounded: a 1 MiB blob with 64
// nesting levels is well above anything a real CMS signature would need
// (typical PKCS#7 signer blocks are a few KiB, nested ~8 levels deep).
// ---------------------------------------------------------------------------

const MAX_INPUT_SIZE: usize = 1024 * 1024;
const MAX_NESTING_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Known OIDs.  The DER encoding of each OID is stored as a byte slice so
// we can compare without re-encoding.
// ---------------------------------------------------------------------------

// 1.2.840.113549.1.7.2 — id-signedData (PKCS#7)
const OID_SIGNED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02];
// 2.5.4.3 — id-at-commonName
const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
// 2.5.4.11 — id-at-organizationalUnitName
const OID_OU: &[u8] = &[0x55, 0x04, 0x0b];
// 2.5.4.10 — id-at-organizationName
const OID_O: &[u8] = &[0x55, 0x04, 0x0a];
// 2.5.4.6 — id-at-countryName
const OID_C: &[u8] = &[0x55, 0x04, 0x06];
// 2.5.4.7 — id-at-localityName
const OID_L: &[u8] = &[0x55, 0x04, 0x07];
// 2.5.4.8 — id-at-stateOrProvinceName
const OID_ST: &[u8] = &[0x55, 0x04, 0x08];
// 2.5.4.9 — id-at-streetAddress
const OID_STREET: &[u8] = &[0x55, 0x04, 0x09];
// 2.5.4.5 — id-at-serialNumber
const OID_SN: &[u8] = &[0x55, 0x04, 0x05];
// 0.9.2342.19200300.100.1.25 — id-domainComponent
const OID_DC: &[u8] = &[0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x19];
// 1.2.840.113549.1.9.1 — emailAddress
const OID_EMAIL: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x01];

// ---------------------------------------------------------------------------
// DER tag constants
// ---------------------------------------------------------------------------

const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_UTF8_STRING: u8 = 0x0c;
const TAG_PRINTABLE_STRING: u8 = 0x13;
const TAG_TELETEX_STRING: u8 = 0x14;
const TAG_IA5_STRING: u8 = 0x16;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
const TAG_BMP_STRING: u8 = 0x1e;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;
const TAG_CONTEXT_0: u8 = 0xa0;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse a PKCS#7 / CMS SignedData blob and return the first signer
/// certificate's Subject as a canonical RFC 4514 DN string.  Returns
/// `X509Error` on any malformed input — never panics.
pub fn parse_signer_dn(pkcs7_der: &[u8]) -> Result<String, X509Error> {
    if pkcs7_der.len() > MAX_INPUT_SIZE {
        return Err(X509Error::TooLarge);
    }
    let cert = extract_first_certificate(pkcs7_der)?;
    let subject = extract_subject(cert)?;
    render_dn(subject)
}

/// Extract the Subject field of an X.509 v1/v2/v3 certificate (as a raw
/// DER slice for the RDNSequence).  Exposed separately so unit tests can
/// build a bare Certificate without wrapping it in PKCS#7.
pub fn parse_cert_subject_dn(cert_der: &[u8]) -> Result<String, X509Error> {
    if cert_der.len() > MAX_INPUT_SIZE {
        return Err(X509Error::TooLarge);
    }
    let subject = extract_subject(cert_der)?;
    render_dn(subject)
}

/// Parse a standalone DER-encoded X.501 `Name` (`SEQUENCE OF
/// RelativeDistinguishedName`, RFC 5280 Appendix A.1) into a canonical
/// RFC 4514 DN string. Unlike [`parse_cert_subject_dn`], the input is the
/// `Name` TLV directly — not a Subject field nested inside a wrapping X.509
/// `Certificate`. This is the shape a TLS `CertificateRequest`'s
/// `certificate_authorities` list carries (rustls's `ResolvesClientCert::
/// resolve` exposes it as `root_hint_subjects: &[&[u8]]`), so the TLS client-
/// cert-selection path (`t27_tls::JavaKeyManagerResolver`) uses this to
/// render the server's acceptable-issuer hints into `Principal.getName()`-
/// compatible strings before handing them to a Java `KeyManager.
/// chooseClientAlias`.
pub fn parse_name_dn(name_der: &[u8]) -> Result<String, X509Error> {
    if name_der.len() > MAX_INPUT_SIZE {
        return Err(X509Error::TooLarge);
    }
    let name = read_tlv_tagged(name_der, TAG_SEQUENCE).map_err(|_| X509Error::MalformedAsn1)?;
    render_dn(name.content)
}

// ---------------------------------------------------------------------------
// ASN.1 DER helpers
// ---------------------------------------------------------------------------

/// A parsed TLV (Tag-Length-Value) triple.  `content` is the raw V bytes;
/// `rest` is everything after the TLV.  We track `depth` to enforce
/// `MAX_NESTING_DEPTH`.
#[derive(Debug)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    rest: &'a [u8],
}

/// Decode one TLV at the start of `input` with the expected tag.  Errors
/// if the tag doesn't match or the length is malformed.
fn read_tlv_tagged<'a>(input: &'a [u8], expected_tag: u8) -> Result<Tlv<'a>, X509Error> {
    let tlv = read_tlv(input)?;
    if tlv.tag != expected_tag {
        return Err(X509Error::MalformedAsn1);
    }
    Ok(tlv)
}

/// Decode one TLV without checking the tag.
fn read_tlv(input: &[u8]) -> Result<Tlv<'_>, X509Error> {
    if input.is_empty() {
        return Err(X509Error::MalformedAsn1);
    }
    let tag = input[0];
    // High-tag-number form (multi-byte tag) is allowed by DER but never
    // appears in certificates; reject for safety.
    if (tag & 0x1f) == 0x1f {
        return Err(X509Error::MalformedAsn1);
    }
    let (len, len_bytes) = read_length(&input[1..])?;
    let header = 1 + len_bytes;
    let end = header.checked_add(len).ok_or(X509Error::MalformedAsn1)?;
    if end > input.len() {
        return Err(X509Error::MalformedAsn1);
    }
    Ok(Tlv {
        tag,
        content: &input[header..end],
        rest: &input[end..],
    })
}

/// Decode a DER length field.  Returns `(length, bytes_consumed)`.
fn read_length(input: &[u8]) -> Result<(usize, usize), X509Error> {
    if input.is_empty() {
        return Err(X509Error::MalformedAsn1);
    }
    let first = input[0];
    if first < 0x80 {
        return Ok((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    // Indefinite length is legal in BER but not in DER.
    if n == 0 {
        return Err(X509Error::MalformedAsn1);
    }
    // Reject unreasonably large length-of-length (anything over 8 bytes
    // guarantees integer overflow on 64-bit platforms).
    if n > 8 || 1 + n > input.len() {
        return Err(X509Error::MalformedAsn1);
    }
    let mut len: usize = 0;
    for i in 0..n {
        len = len
            .checked_shl(8)
            .and_then(|v| v.checked_add(input[1 + i] as usize))
            .ok_or(X509Error::MalformedAsn1)?;
    }
    // Cap length to input-size bound to prevent overflow further down
    // the pipeline.
    if len > MAX_INPUT_SIZE {
        return Err(X509Error::TooLarge);
    }
    Ok((len, 1 + n))
}

// ---------------------------------------------------------------------------
// PKCS#7 extraction
// ---------------------------------------------------------------------------

/// Given a PKCS#7 ContentInfo DER blob, return the DER bytes of the
/// first signer certificate inside its SignedData.
fn extract_first_certificate(pkcs7: &[u8]) -> Result<&[u8], X509Error> {
    // ContentInfo ::= SEQUENCE {
    //     contentType ContentType,          -- id-signedData
    //     content     [0] EXPLICIT SignedData
    // }
    let content_info = read_tlv_tagged(pkcs7, TAG_SEQUENCE).map_err(|_| X509Error::NotPkcs7)?;

    let (content_type_tlv, after_type) =
        split_next(content_info.content, 1).map_err(|_| X509Error::NotPkcs7)?;
    let ct = read_tlv_tagged(content_type_tlv, TAG_OID).map_err(|_| X509Error::NotPkcs7)?;
    if ct.content != OID_SIGNED_DATA {
        return Err(X509Error::NotPkcs7);
    }

    let content_tag =
        read_tlv_tagged(after_type, TAG_CONTEXT_0).map_err(|_| X509Error::NotPkcs7)?;

    // SignedData ::= SEQUENCE {
    //     version Version,
    //     digestAlgorithms DigestAlgorithmIdentifiers,
    //     encapContentInfo EncapsulatedContentInfo,
    //     certificates [0] IMPLICIT CertificateSet OPTIONAL,
    //     crls         [1] IMPLICIT RevocationInfoChoices OPTIONAL,
    //     signerInfos  SignerInfos
    // }
    let signed_data =
        read_tlv_tagged(content_tag.content, TAG_SEQUENCE).map_err(|_| X509Error::NotPkcs7)?;
    let mut cursor = signed_data.content;

    // Skip version (INTEGER).
    let v = read_tlv(cursor).map_err(|_| X509Error::NotPkcs7)?;
    if v.tag != TAG_INTEGER {
        return Err(X509Error::NotPkcs7);
    }
    cursor = v.rest;

    // Skip digestAlgorithms (SET).
    let da = read_tlv(cursor).map_err(|_| X509Error::NotPkcs7)?;
    if da.tag != TAG_SET {
        return Err(X509Error::NotPkcs7);
    }
    cursor = da.rest;

    // Skip encapContentInfo (SEQUENCE).
    let eci = read_tlv(cursor).map_err(|_| X509Error::NotPkcs7)?;
    if eci.tag != TAG_SEQUENCE {
        return Err(X509Error::NotPkcs7);
    }
    cursor = eci.rest;

    // Now look for certificates [0] IMPLICIT.  Context-specific-0
    // constructed is tag 0xa0.
    while !cursor.is_empty() {
        let tlv = read_tlv(cursor).map_err(|_| X509Error::MalformedAsn1)?;
        if tlv.tag == TAG_CONTEXT_0 {
            // certificates SET: each entry is a Certificate SEQUENCE.
            let first =
                read_tlv_tagged(tlv.content, TAG_SEQUENCE).map_err(|_| X509Error::NoSignerCert)?;
            // Return the SEQUENCE including its own header — callers
            // parse it again as a standalone Certificate.
            let consumed = tlv.content.len() - first.rest.len();
            return Ok(&tlv.content[..consumed]);
        }
        cursor = tlv.rest;
    }
    Err(X509Error::NoSignerCert)
}

/// Helper: split off the first TLV from `buf`; returns `(tlv_bytes, rest_bytes)`.
/// `_max_depth` is a placeholder for future recursive use.
fn split_next(buf: &[u8], _max_depth: usize) -> Result<(&[u8], &[u8]), X509Error> {
    let tlv = read_tlv(buf)?;
    let consumed = buf.len() - tlv.rest.len();
    Ok((&buf[..consumed], tlv.rest))
}

// ---------------------------------------------------------------------------
// X.509 Certificate → Subject
// ---------------------------------------------------------------------------

/// Given a DER-encoded X.509 Certificate, return the raw bytes of the
/// Subject RDNSequence (without its outer SEQUENCE tag).
fn extract_subject(cert_der: &[u8]) -> Result<&[u8], X509Error> {
    // Certificate ::= SEQUENCE {
    //     tbsCertificate TBSCertificate,
    //     signatureAlgorithm AlgorithmIdentifier,
    //     signatureValue BIT STRING
    // }
    let cert = read_tlv_tagged(cert_der, TAG_SEQUENCE).map_err(|_| X509Error::MalformedCert)?;
    let tbs = read_tlv_tagged(cert.content, TAG_SEQUENCE).map_err(|_| X509Error::MalformedCert)?;

    // TBSCertificate ::= SEQUENCE {
    //     version         [0] EXPLICIT Version DEFAULT v1,
    //     serialNumber    CertificateSerialNumber,
    //     signature       AlgorithmIdentifier,
    //     issuer          Name,
    //     validity        Validity,
    //     subject         Name,            ← we want this
    //     subjectPublicKeyInfo SubjectPublicKeyInfo,
    //     ...
    // }
    let mut cursor = tbs.content;

    // Optional [0] EXPLICIT version.
    let first = read_tlv(cursor).map_err(|_| X509Error::MalformedCert)?;
    if first.tag == TAG_CONTEXT_0 {
        cursor = first.rest;
    }

    // serialNumber INTEGER
    let sn = read_tlv(cursor).map_err(|_| X509Error::MalformedCert)?;
    if sn.tag != TAG_INTEGER {
        return Err(X509Error::MalformedCert);
    }
    cursor = sn.rest;

    // signature AlgorithmIdentifier SEQUENCE
    let alg = read_tlv(cursor).map_err(|_| X509Error::MalformedCert)?;
    if alg.tag != TAG_SEQUENCE {
        return Err(X509Error::MalformedCert);
    }
    cursor = alg.rest;

    // issuer Name SEQUENCE (skip)
    let issuer = read_tlv(cursor).map_err(|_| X509Error::MalformedCert)?;
    if issuer.tag != TAG_SEQUENCE {
        return Err(X509Error::MalformedCert);
    }
    cursor = issuer.rest;

    // validity SEQUENCE (skip)
    let validity = read_tlv(cursor).map_err(|_| X509Error::MalformedCert)?;
    if validity.tag != TAG_SEQUENCE {
        return Err(X509Error::MalformedCert);
    }
    cursor = validity.rest;

    // subject Name SEQUENCE
    let subject = read_tlv_tagged(cursor, TAG_SEQUENCE).map_err(|_| X509Error::NoSubject)?;
    if subject.content.is_empty() {
        return Err(X509Error::NoSubject);
    }
    Ok(subject.content)
}

// ---------------------------------------------------------------------------
// RDNSequence → String
// ---------------------------------------------------------------------------

/// Render an RDNSequence (the inner bytes of Subject's SEQUENCE) as a
/// canonical RFC 4514 DN string.
fn render_dn(rdn_sequence: &[u8]) -> Result<String, X509Error> {
    let rdns = collect_rdns(rdn_sequence, 0)?;
    if rdns.is_empty() {
        return Err(X509Error::NoSubject);
    }
    // RFC 4514: output in reverse order (most-specific first).
    let mut parts: Vec<String> = Vec::with_capacity(rdns.len());
    for rdn in rdns.iter().rev() {
        parts.push(rdn.clone());
    }
    Ok(parts.join(", "))
}

/// Decode every RDN in an RDNSequence into a rendered `"Type=Value"`
/// string (or `"A=x+B=y"` for multi-valued RDNs).
fn collect_rdns(buf: &[u8], depth: usize) -> Result<Vec<String>, X509Error> {
    if depth > MAX_NESTING_DEPTH {
        return Err(X509Error::TooLarge);
    }
    let mut out = Vec::new();
    let mut cursor = buf;
    while !cursor.is_empty() {
        let rdn = read_tlv_tagged(cursor, TAG_SET).map_err(|_| X509Error::MalformedCert)?;
        out.push(render_rdn(rdn.content, depth + 1)?);
        cursor = rdn.rest;
    }
    Ok(out)
}

/// Render one RDN (SET of AttributeTypeAndValue) as a string.
fn render_rdn(buf: &[u8], depth: usize) -> Result<String, X509Error> {
    if depth > MAX_NESTING_DEPTH {
        return Err(X509Error::TooLarge);
    }
    let mut atvs: Vec<String> = Vec::new();
    let mut cursor = buf;
    while !cursor.is_empty() {
        let atv = read_tlv_tagged(cursor, TAG_SEQUENCE).map_err(|_| X509Error::MalformedCert)?;
        atvs.push(render_atv(atv.content, depth + 1)?);
        cursor = atv.rest;
    }
    Ok(atvs.join("+"))
}

/// Render one AttributeTypeAndValue (SEQUENCE { OID, ANY }) as
/// `"CN=Acme"` / `"OID.1.2.3=\\0A..."`.
fn render_atv(buf: &[u8], depth: usize) -> Result<String, X509Error> {
    if depth > MAX_NESTING_DEPTH {
        return Err(X509Error::TooLarge);
    }
    let oid = read_tlv_tagged(buf, TAG_OID).map_err(|_| X509Error::MalformedCert)?;
    let value = read_tlv(oid.rest).map_err(|_| X509Error::MalformedCert)?;
    let type_label = oid_label(oid.content);
    let text = decode_directory_string(value.tag, value.content);
    Ok(format!("{}={}", type_label, escape_rdn_value(&text)))
}

/// Map a known OID to its RFC 4514 short name.  Unknown OIDs render as
/// `OID.x.y.z` using the dotted-decimal form.
fn oid_label(oid_bytes: &[u8]) -> String {
    match oid_bytes {
        b if b == OID_CN => "CN".to_string(),
        b if b == OID_OU => "OU".to_string(),
        b if b == OID_O => "O".to_string(),
        b if b == OID_C => "C".to_string(),
        b if b == OID_L => "L".to_string(),
        b if b == OID_ST => "ST".to_string(),
        b if b == OID_STREET => "STREET".to_string(),
        b if b == OID_SN => "SERIALNUMBER".to_string(),
        b if b == OID_DC => "DC".to_string(),
        b if b == OID_EMAIL => "EMAILADDRESS".to_string(),
        _ => format!("OID.{}", oid_to_string(oid_bytes)),
    }
}

/// Decode the DER-encoded OID bytes into dotted-decimal text.  Never
/// fails — malformed OIDs emit a hex placeholder with the `?.` prefix.
fn oid_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "?".to_string();
    }
    let first = bytes[0];
    let arc1 = (first / 40) as u32;
    let arc2 = (first % 40) as u32;
    let mut out = format!("{}.{}", arc1, arc2);
    let mut acc: u64 = 0;
    for &b in &bytes[1..] {
        // Each subsequent arc: 7 bits per byte, MSB is continuation.
        acc = match acc.checked_shl(7) {
            Some(v) => v,
            None => return format!("?.{}", hex_encode(bytes)),
        };
        acc |= (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            out.push('.');
            out.push_str(&acc.to_string());
            acc = 0;
        }
    }
    out
}

/// Decode an AttributeValue's content according to its tag.  Falls back
/// to a hex dump when the tag is unrecognised so the DN string still
/// renders something human-readable.
fn decode_directory_string(tag: u8, content: &[u8]) -> String {
    match tag {
        TAG_PRINTABLE_STRING | TAG_IA5_STRING | TAG_UTF8_STRING => std::str::from_utf8(content)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| format!("#{}", hex_encode(content))),
        TAG_TELETEX_STRING => {
            // T61/TeletexString is mostly ASCII in practice; fall back
            // to Latin-1 for anything outside ASCII so mojibake is at
            // least non-lossy.
            let mut s = String::with_capacity(content.len());
            for &b in content {
                s.push(b as char);
            }
            s
        }
        TAG_BMP_STRING => {
            // UCS-2 big-endian.
            if content.len() % 2 != 0 {
                return format!("#{}", hex_encode(content));
            }
            let mut s = String::with_capacity(content.len() / 2);
            for pair in content.chunks_exact(2) {
                let code = u16::from_be_bytes([pair[0], pair[1]]);
                match char::from_u32(code as u32) {
                    Some(c) => s.push(c),
                    None => return format!("#{}", hex_encode(content)),
                }
            }
            s
        }
        TAG_UTC_TIME | TAG_GENERALIZED_TIME | TAG_OCTET_STRING | TAG_BIT_STRING => {
            format!("#{}", hex_encode(content))
        }
        _ => {
            // Unknown or relative OID tag — hex-encode so we never panic.
            format!("#{}", hex_encode(content))
        }
    }
}

/// Lower-case hex of a byte slice (no separator).
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Apply RFC 4514 §2.4 escaping to the value portion of an RDN.
fn escape_rdn_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    for (i, ch) in s.chars().enumerate() {
        let is_first = i == 0;
        let is_last = i + ch.len_utf8() == bytes.len();
        match ch {
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '#' if is_first => {
                out.push('\\');
                out.push('#');
            }
            ' ' if is_first || is_last => {
                out.push('\\');
                out.push(' ');
            }
            '\0' => {
                out.push_str("\\00");
            }
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Test helpers (only compiled for tests) — a minimal DER encoder so we
// can build Certificate / PKCS#7 fixtures by hand without a third-party
// crate.  Kept inside the module so the parser and encoder never drift.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(super) mod builder {
    use super::*;

    /// Emit a DER TLV with the given tag and body.
    pub fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 8);
        out.push(tag);
        encode_length(body.len(), &mut out);
        out.extend_from_slice(body);
        out
    }

    fn encode_length(len: usize, out: &mut Vec<u8>) {
        if len < 0x80 {
            out.push(len as u8);
            return;
        }
        // Emit the shortest big-endian length.
        let mut bytes = Vec::new();
        let mut v = len;
        while v > 0 {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
        }
        bytes.reverse();
        out.push(0x80 | (bytes.len() as u8));
        out.extend_from_slice(&bytes);
    }

    pub fn sequence(body: &[u8]) -> Vec<u8> {
        tlv(TAG_SEQUENCE, body)
    }

    pub fn set(body: &[u8]) -> Vec<u8> {
        tlv(TAG_SET, body)
    }

    pub fn oid(raw: &[u8]) -> Vec<u8> {
        tlv(TAG_OID, raw)
    }

    pub fn printable_string(s: &str) -> Vec<u8> {
        tlv(TAG_PRINTABLE_STRING, s.as_bytes())
    }

    pub fn utf8_string(s: &str) -> Vec<u8> {
        tlv(TAG_UTF8_STRING, s.as_bytes())
    }

    pub fn ia5_string(s: &str) -> Vec<u8> {
        tlv(TAG_IA5_STRING, s.as_bytes())
    }

    pub fn integer(n: u32) -> Vec<u8> {
        let mut bytes = n.to_be_bytes().to_vec();
        while bytes.len() > 1 && bytes[0] == 0 && (bytes[1] & 0x80) == 0 {
            bytes.remove(0);
        }
        tlv(TAG_INTEGER, &bytes)
    }

    pub fn context0(body: &[u8]) -> Vec<u8> {
        tlv(TAG_CONTEXT_0, body)
    }

    /// Build a minimal AlgorithmIdentifier (OID + NULL) whose OID is
    /// arbitrary — the parser never looks at it, only skips it.
    pub fn algorithm_identifier() -> Vec<u8> {
        // SEQUENCE { OID 1.2.840.113549.1.1.11 (sha256WithRSAEncryption), NULL }
        let oid_bytes = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
        let mut body = oid(&oid_bytes);
        // NULL: tag 0x05, length 0.
        body.extend_from_slice(&[0x05, 0x00]);
        sequence(&body)
    }

    pub fn validity() -> Vec<u8> {
        // SEQUENCE { UTCTime "990101000000Z", UTCTime "491231235959Z" }
        let not_before = tlv(TAG_UTC_TIME, b"990101000000Z");
        let not_after = tlv(TAG_UTC_TIME, b"491231235959Z");
        let mut body = Vec::new();
        body.extend_from_slice(&not_before);
        body.extend_from_slice(&not_after);
        sequence(&body)
    }

    /// Subject Public Key Info: SEQUENCE { AlgorithmIdentifier, BIT STRING }
    pub fn spki() -> Vec<u8> {
        let mut body = algorithm_identifier();
        let bit_string = tlv(TAG_BIT_STRING, &[0x00]);
        body.extend_from_slice(&bit_string);
        sequence(&body)
    }

    /// Build an `AttributeTypeAndValue` SEQUENCE{OID, String}.
    pub fn atv(oid_bytes: &[u8], value: &[u8]) -> Vec<u8> {
        let mut body = oid(oid_bytes);
        body.extend_from_slice(value);
        sequence(&body)
    }

    /// Single-valued RDN: SET of one ATV.
    pub fn rdn(atv_bytes: &[u8]) -> Vec<u8> {
        set(atv_bytes)
    }

    pub fn name(rdns: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for r in rdns {
            body.extend_from_slice(r);
        }
        sequence(&body)
    }

    /// Build a minimal X.509 v3 certificate with the given subject DN
    /// (as an RDNSequence body) and an arbitrary issuer.  Suitable for
    /// feeding to `parse_cert_subject_dn`.
    pub fn certificate(subject_name: &[u8]) -> Vec<u8> {
        let issuer = name(&[rdn(&atv(OID_CN, &printable_string("Issuer")))]);
        let mut tbs_body = Vec::new();
        // version [0] EXPLICIT INTEGER 2 (v3)
        tbs_body.extend_from_slice(&context0(&integer(2)));
        // serialNumber
        tbs_body.extend_from_slice(&integer(1));
        // signature AlgorithmIdentifier
        tbs_body.extend_from_slice(&algorithm_identifier());
        // issuer
        tbs_body.extend_from_slice(&issuer);
        // validity
        tbs_body.extend_from_slice(&validity());
        // subject (raw Name bytes already include the SEQUENCE wrapper)
        tbs_body.extend_from_slice(subject_name);
        // subjectPublicKeyInfo
        tbs_body.extend_from_slice(&spki());
        let tbs = sequence(&tbs_body);

        let mut cert_body = tbs;
        cert_body.extend_from_slice(&algorithm_identifier());
        let signature = tlv(TAG_BIT_STRING, &[0x00]);
        cert_body.extend_from_slice(&signature);
        sequence(&cert_body)
    }

    /// Wrap a single Certificate in a PKCS#7 ContentInfo/SignedData envelope.
    pub fn pkcs7_signed_data(cert_der: &[u8]) -> Vec<u8> {
        // digestAlgorithms SET OF AlgorithmIdentifier — empty set is fine
        // for parsing purposes.
        let digest_algorithms = set(&[]);

        // EncapsulatedContentInfo SEQUENCE { contentType OID }
        let encap = sequence(&oid(&[
            0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01,
        ]));

        // certificates [0] IMPLICIT { Certificate }
        let certificates = tlv(TAG_CONTEXT_0, cert_der);

        // signerInfos SET — empty.
        let signer_infos = set(&[]);

        let mut signed_data_body = Vec::new();
        signed_data_body.extend_from_slice(&integer(1));
        signed_data_body.extend_from_slice(&digest_algorithms);
        signed_data_body.extend_from_slice(&encap);
        signed_data_body.extend_from_slice(&certificates);
        signed_data_body.extend_from_slice(&signer_infos);
        let signed_data = sequence(&signed_data_body);

        // Wrap in ContentInfo.
        let content_info_body = {
            let mut body = oid(OID_SIGNED_DATA);
            body.extend_from_slice(&context0(&signed_data));
            body
        };
        sequence(&content_info_body)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::builder::*;
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn subject_cn(name_text: &str) -> Vec<u8> {
        name(&[rdn(&atv(OID_CN, &printable_string(name_text)))])
    }

    #[test]
    fn parse_bare_cert_cn_only() {
        let cert = certificate(&subject_cn("Acme Corp"));
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert_eq!(dn, "CN=Acme Corp");
    }

    #[test]
    fn parse_multi_rdn_reverse_order() {
        // Subject C=US, O=Acme, CN=Widget — renders most-specific first.
        let rdns = vec![
            rdn(&atv(OID_C, &printable_string("US"))),
            rdn(&atv(OID_O, &printable_string("Acme"))),
            rdn(&atv(OID_CN, &printable_string("Widget"))),
        ];
        let cert = certificate(&name(&rdns));
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert_eq!(dn, "CN=Widget, O=Acme, C=US");
    }

    #[test]
    fn parse_utf8_value() {
        let subj = name(&[rdn(&atv(OID_CN, &utf8_string("Ümlaut GmbH")))]);
        let cert = certificate(&subj);
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert_eq!(dn, "CN=Ümlaut GmbH");
    }

    #[test]
    fn parse_unknown_oid_renders_dotted() {
        // Random synthetic OID 1.2.3.4.5 = 0x2a 0x03 0x04 0x05.
        let unknown_oid = [0x2a, 0x03, 0x04, 0x05];
        let subj = name(&[rdn(&atv(&unknown_oid, &printable_string("x")))]);
        let cert = certificate(&subj);
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert!(dn.starts_with("OID.1.2.3.4.5="), "got {dn}");
    }

    #[test]
    fn parse_pkcs7_wrapped() {
        let cert = certificate(&subject_cn("Acme Corp"));
        let p7 = pkcs7_signed_data(&cert);
        let dn = parse_signer_dn(&p7).unwrap();
        assert_eq!(dn, "CN=Acme Corp");
    }

    #[test]
    fn rfc4514_escapes_special_chars() {
        let subj = name(&[rdn(&atv(OID_CN, &utf8_string("Acme, Inc.")))]);
        let cert = certificate(&subj);
        let dn = parse_cert_subject_dn(&cert).unwrap();
        // Comma must be backslash-escaped inside the value.
        assert_eq!(dn, "CN=Acme\\, Inc.");
    }

    #[test]
    fn rfc4514_escapes_leading_space_and_hash() {
        let subj = name(&[rdn(&atv(OID_CN, &utf8_string(" Acme")))]);
        let cert = certificate(&subj);
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert_eq!(dn, "CN=\\ Acme");

        let subj2 = name(&[rdn(&atv(OID_CN, &utf8_string("#Acme")))]);
        let cert2 = certificate(&subj2);
        let dn2 = parse_cert_subject_dn(&cert2).unwrap();
        assert_eq!(dn2, "CN=\\#Acme");
    }

    #[test]
    fn malformed_input_returns_err_not_panic() {
        assert!(parse_signer_dn(b"").is_err());
        assert!(parse_signer_dn(&[0x30, 0x82, 0xff, 0xff]).is_err());
        assert!(parse_signer_dn(&[0xff; 256]).is_err());
        // Truncated TLV.
        assert!(parse_signer_dn(&[0x30, 0x05, 0x02]).is_err());
    }

    #[test]
    fn oversize_input_rejected() {
        let big = vec![0u8; MAX_INPUT_SIZE + 1];
        assert_eq!(parse_signer_dn(&big).unwrap_err(), X509Error::TooLarge);
    }

    #[test]
    fn indefinite_length_rejected() {
        // BER indefinite length (first byte 0x80) is illegal in DER.
        let bad = vec![0x30, 0x80, 0x00, 0x00];
        assert_eq!(parse_signer_dn(&bad).unwrap_err(), X509Error::NotPkcs7);
    }

    #[test]
    fn cert_without_signed_data_oid_rejected() {
        let cert = certificate(&subject_cn("Acme"));
        // Feed a bare Certificate (no PKCS#7 wrapper) — parse_signer_dn
        // must reject it.
        let err = parse_signer_dn(&cert).unwrap_err();
        assert_eq!(err, X509Error::NotPkcs7);
    }

    #[test]
    fn multi_valued_rdn_joined_with_plus() {
        // SET { ATV1, ATV2 }
        let mut set_body = Vec::new();
        set_body.extend_from_slice(&atv(OID_CN, &printable_string("Foo")));
        set_body.extend_from_slice(&atv(OID_O, &printable_string("Bar")));
        let r = set(&set_body);
        let subj = name(&[r]);
        let cert = certificate(&subj);
        let dn = parse_cert_subject_dn(&cert).unwrap();
        // DER SET is sorted so ordering is not stable, but both must
        // be present joined with '+'.
        assert!(dn.contains("CN=Foo"));
        assert!(dn.contains("O=Bar"));
        assert!(dn.contains("+"));
    }

    #[test]
    fn email_and_ou_render() {
        let rdns = vec![
            rdn(&atv(OID_CN, &printable_string("John"))),
            rdn(&atv(OID_OU, &printable_string("Eng"))),
            rdn(&atv(OID_EMAIL, &ia5_string("john@acme.com"))),
        ];
        let cert = certificate(&name(&rdns));
        let dn = parse_cert_subject_dn(&cert).unwrap();
        assert!(dn.contains("CN=John"), "got {dn}");
        assert!(dn.contains("OU=Eng"), "got {dn}");
        assert!(dn.contains("EMAILADDRESS=john@acme.com"), "got {dn}");
    }
}
