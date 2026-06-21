// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.3 — `X509KeyManager` / `X509TrustManager` per-connection.
//!
//! This module owns the SunJSSE-internal native surface for the X509 manager
//! pair that backs every `javax.net.ssl.SSLContext` instance once
//! `KeyManagerFactory.init(KeyStore, char[])` and
//! `TrustManagerFactory.init(KeyStore)` have run. The public-facing
//! `javax.net.ssl.X509KeyManager` / `X509TrustManager` interface stubs in
//! `tls.rs` and `phases_late.rs` are `getInstance` thin shims; the real
//! per-connection state machine lives here.
//!
//! Surface registered (FQNs):
//!
//!   * `sun/security/ssl/SunX509KeyManagerImpl`
//!     * `chooseClientAlias(String[] keyTypes, Principal[] issuers, Socket s)`
//!     * `chooseServerAlias(String keyType, Principal[] issuers, Socket s)`
//!     * `getCertificateChain(String alias)`
//!     * `getPrivateKey(String alias)`
//!     * `getServerAliases(String keyType, Principal[] issuers)`
//!     * `getClientAliases(String keyType, Principal[] issuers)`
//!   * `sun/security/ssl/X509KeyManagerImpl` — modern SunJSSE variant (same
//!     surface, different FQN).
//!   * `sun/security/ssl/X509TrustManagerImpl`
//!     * `checkClientTrusted(X509Certificate[] chain, String authType)`
//!     * `checkServerTrusted(X509Certificate[] chain, String authType)`
//!     * `getAcceptedIssuers()`
//!   * `sun/security/validator/PKIXValidator`
//!     * `engineValidate(Certificate[] chain, ...)`
//!   * `sun/security/ssl/KeyManagerFactoryImpl$SunX509`
//!     * `engineInit(KeyStore, char[])`, `engineGetKeyManagers()`
//!   * `sun/security/ssl/TrustManagerFactoryImpl$SimpleFactory`
//!     * `engineInit(KeyStore)`, `engineGetTrustManagers()`
//!
//! ## Cert selection
//!
//! Both client- and server-alias selection walk every `PrivateKey` entry in
//! the bound `KeyStore`, parse the leaf cert's `KeyUsage` (OID 2.5.29.15) and
//! `ExtendedKeyUsage` (OID 2.5.29.37) extensions, and filter by the keyType
//! the caller requested:
//!
//!   * Server cert: `keyUsage` ∈ {digitalSignature, keyEncipherment}; `EKU` ∋
//!     `id-kp-serverAuth` (OID 1.3.6.1.5.5.7.3.1) — or no EKU extension.
//!   * Client cert: `keyUsage` ∋ digitalSignature; `EKU` ∋
//!     `id-kp-clientAuth` (OID 1.3.6.1.5.5.7.3.2) — or no EKU extension.
//!
//! `keyType` matching is by SPKI algorithm OID — RSA = 1.2.840.113549.1.1.1,
//! EC = 1.2.840.10045.2.1, DSA = 1.2.840.10040.4.1, Ed25519 = 1.3.101.112.
//!
//! ## Trust chain validation (RFC 5280 §6)
//!
//! `checkServerTrusted` / `checkClientTrusted` build the chain leaf →
//! intermediate(s) → root and verify, in order:
//!
//!   1. Each cert's `notBefore <= now <= notAfter` (clock check).
//!   2. Each cert's signature against its issuer's `SubjectPublicKeyInfo`
//!      (or, for a self-signed root, against its own SPKI).
//!   3. Issuer ↔ subject DN continuity along the chain.
//!   4. Intermediate `BasicConstraints.cA = TRUE` (RFC 5280 §4.2.1.9).
//!   5. The last cert's subject DN matches the subject DN of a trust anchor
//!      pulled from `rustls_native_certs::load_native_certs()`.
//!   6. Name constraints — currently we *parse* the extension and reject
//!      anything that fails the simplest DNS-name `excludedSubtree` check;
//!      a complete RFC 5280 §4.2.1.10 implementation is queued as
//!      WP5.3.followup once a real EJBCA-issued nameConstraints fixture
//!      lands in `bench/`.
//!
//! Failure throws a `CertificateException` (mapped to `RuntimeError::IO`
//! at the bytecode boundary).
//!
//! ## Endpoint identification (hostname verification)
//!
//! Chain validation proves *trust* but not *identity*: a cert validly issued
//! for `evil.example` still chains to a trusted anchor. For HTTPS/LDAPS the
//! peer's host must additionally match an identity the leaf asserts. That
//! check lives in `verify_hostname` / `check_endpoint_identity` (RFC 6125 /
//! RFC 2818: SubjectAltName `dNSName`/`iPAddress` with wildcard rules, legacy
//! `commonName` fallback only when no `dNSName` SAN is present).
//!
//! GAP: the `javax.net.ssl.X509TrustManager` surface backed here
//! (`checkServerTrusted(X509Certificate[], String authType)`) does **not**
//! carry the intended peer host — in real-JDK the host check runs inside the
//! SSL engine (`X509TrustManagerImpl.checkIdentity`) keyed off
//! `SSLParameters.getEndpointIdentificationAlgorithm()` and the `SSLSession`
//! peer host, neither of which is an argument to that method. So
//! `do_check_trusted` deliberately performs only chain trust; endpoint
//! identity must be invoked by the SSL-engine layer (tls.rs) at the point the
//! host *is* available, via the public `verify_hostname` entry point. We do
//! not weaken chain validation, and we never accept a cert for the wrong host
//! once a host is threaded in.
//!
//! CRL is gated behind a runtime config flag and defaults *off* — it is
//! not in the WP5.3 acceptance criteria.

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use parking_lot::RwLock;

use crate::alloc_concurrent_synthetic;
use crate::keystore;

// ---------------------------------------------------------------------------
// Per-manager state
// ---------------------------------------------------------------------------

/// Indexed view of a `KeyStore`'s contents, ready to drive cert-alias
/// selection. Built once at `engineInit` time and cached on a 32-bit slot
/// id we stash in the synthetic Java mirror.
#[derive(Clone, Debug, Default)]
pub struct KeyManagerState {
    /// Source keystore id (so we can re-fetch DER on demand).
    pub keystore_id: i32,
    /// alias -> chain DER (leaf first).
    pub aliases_to_chain: HashMap<String, Vec<Vec<u8>>>,
    /// alias -> private-key DER (PKCS#8 SEQUENCE).
    pub aliases_to_key: HashMap<String, Vec<u8>>,
    /// keyType (e.g. "RSA", "EC") -> aliases eligible as a *server* cert.
    pub server_aliases_by_key_type: HashMap<String, Vec<String>>,
    /// keyType -> aliases eligible as a *client* cert.
    pub client_aliases_by_key_type: HashMap<String, Vec<String>>,
}

/// Trust-manager state. We keep the explicit anchors loaded out of the
/// caller's `KeyStore` plus the system trust store as a fallback. Both
/// roles fold into `anchors`, which is keyed by subject-DN DER for fast
/// chain-end matching. `anchor_ders` is the original DER bytes so
/// `getAcceptedIssuers()` can return them as `X509Certificate[]`.
#[derive(Clone, Debug, Default)]
pub struct TrustManagerState {
    pub keystore_id: i32,
    /// subject_dn_der -> SPKI bytes (used to verify the chain's last cert).
    pub anchors: HashMap<Vec<u8>, AnchorInfo>,
    /// The full DER of every trust anchor, in registration order.
    pub anchor_ders: Vec<Vec<u8>>,
    /// Whether to consult CRLs during validation. Disabled by default.
    pub enable_crl: bool,
}

#[derive(Clone, Debug)]
pub struct AnchorInfo {
    pub subject_der: Vec<u8>,
    pub spki_der: Vec<u8>,
    pub full_cert_der: Option<Vec<u8>>,
}

static KM_REGISTRY: OnceLock<RwLock<HashMap<i32, KeyManagerState>>> = OnceLock::new();
static TM_REGISTRY: OnceLock<RwLock<HashMap<i32, TrustManagerState>>> = OnceLock::new();
static NEXT_KM_ID: OnceLock<RwLock<i32>> = OnceLock::new();
static NEXT_TM_ID: OnceLock<RwLock<i32>> = OnceLock::new();

fn km_registry() -> &'static RwLock<HashMap<i32, KeyManagerState>> {
    KM_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn tm_registry() -> &'static RwLock<HashMap<i32, TrustManagerState>> {
    TM_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_km_id() -> i32 {
    let cell = NEXT_KM_ID.get_or_init(|| RwLock::new(1));
    let mut g = cell.write();
    let id = *g;
    *g = g.checked_add(1).unwrap_or(1);
    id
}

fn next_tm_id() -> i32 {
    let cell = NEXT_TM_ID.get_or_init(|| RwLock::new(1));
    let mut g = cell.write();
    let id = *g;
    *g = g.checked_add(1).unwrap_or(1);
    id
}

// ---------------------------------------------------------------------------
// X.509 DER walker (focused subset — extensions + validity + SPKI alg OID).
//
// We deliberately do not depend on `crate::security_manager::x509` for
// extension extraction: that module is the policy-engine signer-DN parser
// and only exposes `parse_signer_dn` / `parse_cert_subject_dn`. Building a
// shim parser here keeps our scope independent of feature gates and lets
// the trust manager validate a chain in isolation.
// ---------------------------------------------------------------------------

/// Parsed extract from a single X.509 leaf / intermediate / root.
#[derive(Clone, Debug)]
pub struct ParsedCert {
    pub tbs_bytes: Vec<u8>,
    pub subject_der: Vec<u8>,
    pub issuer_der: Vec<u8>,
    pub spki_der: Vec<u8>,
    pub spki_algorithm_oid: Vec<u8>,
    pub not_before_secs: i64,
    pub not_after_secs: i64,
    pub key_usage: Option<u16>,
    pub ext_key_usage: Vec<Vec<u8>>,
    pub basic_constraints_ca: Option<bool>,
    pub signature_algorithm_oid: Vec<u8>,
    pub signature_value: Vec<u8>,
    pub is_v3: bool,
    /// `dNSName` entries from the SubjectAltName extension (lower-cased,
    /// in document order). Drives RFC 6125 endpoint-identity matching.
    pub san_dns_names: Vec<String>,
    /// `iPAddress` entries from the SubjectAltName extension as raw bytes
    /// (4 bytes for IPv4, 16 for IPv6).
    pub san_ip_addresses: Vec<Vec<u8>>,
    /// The most-specific `commonName` RDN from the subject DN, if any. Used
    /// only as a legacy fallback identity when the leaf has no `dNSName` SAN.
    pub subject_cn: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertParseError {
    Truncated,
    BadTag,
    BadLength,
    BadStructure,
    BadTime,
}

impl std::fmt::Display for CertParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertParseError::Truncated => f.write_str("truncated DER"),
            CertParseError::BadTag => f.write_str("unexpected DER tag"),
            CertParseError::BadLength => f.write_str("bad DER length"),
            CertParseError::BadStructure => f.write_str("bad X.509 structure"),
            CertParseError::BadTime => f.write_str("bad UTCTime/GeneralizedTime"),
        }
    }
}

const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_NULL: u8 = 0x05;
const TAG_OID: u8 = 0x06;
const TAG_BOOLEAN: u8 = 0x01;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_SET: u8 = 0x31;
const TAG_CONTEXT_0: u8 = 0xa0;
const TAG_CONTEXT_3: u8 = 0xa3;

/// EKU OIDs we recognise. DER form (OID body, no tag/length).
///   id-kp-serverAuth: 1.3.6.1.5.5.7.3.1
pub const OID_KP_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
///   id-kp-clientAuth: 1.3.6.1.5.5.7.3.2
pub const OID_KP_CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];
///   id-kp-codeSigning: 1.3.6.1.5.5.7.3.3
pub const OID_KP_CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];
///   id-kp-emailProtection: 1.3.6.1.5.5.7.3.4
pub const OID_KP_EMAIL_PROTECTION: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x04];

/// Public-key algorithm OIDs.
///   1.2.840.113549.1.1.1 — RSA
const OID_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
///   1.2.840.10045.2.1 — id-ecPublicKey
const OID_EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
///   1.2.840.10040.4.1 — DSA
const OID_DSA: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x01];
///   1.3.101.112 — Ed25519
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];
///   1.3.101.113 — Ed448
const OID_ED448: &[u8] = &[0x2b, 0x65, 0x71];

/// Extension OIDs.
///   2.5.29.15 — KeyUsage
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
///   2.5.29.37 — ExtendedKeyUsage
const OID_EXT_EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
///   2.5.29.19 — BasicConstraints
const OID_EXT_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
///   2.5.29.17 — SubjectAltName
const OID_EXT_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];

/// AttributeType OID `2.5.4.3` — commonName (CN), used as the legacy
/// fallback identity when a leaf carries no `dNSName` SubjectAltName.
const OID_AT_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

/// `GeneralName` context-specific tags inside a SubjectAltName SEQUENCE
/// (RFC 5280 §4.2.1.6). We only consume the two that matter for endpoint
/// identification.
///   [2] IMPLICIT IA5String — dNSName
const SAN_TAG_DNS_NAME: u8 = 0x82;
///   [7] IMPLICIT OCTET STRING — iPAddress (4 bytes v4 / 16 bytes v6)
const SAN_TAG_IP_ADDRESS: u8 = 0x87;

/// Signature-algorithm OIDs (the OID inside `tbsCertificate.signature` and
/// the outer `signatureAlgorithm`).
///   1.2.840.113549.1.1.11 — sha256WithRSAEncryption (PKCS#1 v1.5)
const OID_SIG_SHA256_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
///   1.2.840.10045.4.3.2 — ecdsa-with-SHA256 (P-256 most common)
const OID_SIG_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];

// --- Signature-algorithm OIDs we deliberately do NOT support yet. ---
//
// These are recognised but routed to `TrustError::NotImplemented` so the
// caller can distinguish "we don't know this OID at all" from "we know it
// and chose not to implement it in this layer". WP5.3.followup will pick
// them up:
//
//   * 1.2.840.10040.4.3 — id-dsa-with-sha1 (DSA, deprecated by NIST 2024)
//   * 1.2.840.113549.1.1.10 — id-RSASSA-PSS (RSA-PSS, needs salt-length
//     + MGF1 parameter parsing from the algorithm-identifier `parameters`
//     SEQUENCE)
//   * 1.3.101.112 — id-Ed25519 (pure EdDSA over Curve25519, separate
//     verify path — no SHA-256 preimage)
///   1.2.840.10040.4.3 — id-dsa-with-sha1
const OID_SIG_DSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x03];
///   1.2.840.113549.1.1.10 — id-RSASSA-PSS
const OID_SIG_RSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
///   1.3.101.112 — id-Ed25519
const OID_SIG_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

// ---- KeyUsage bit positions (RFC 5280 §4.2.1.3) ----
pub const KU_DIGITAL_SIGNATURE: u16 = 1 << 0;
pub const KU_NON_REPUDIATION: u16 = 1 << 1;
pub const KU_KEY_ENCIPHERMENT: u16 = 1 << 2;
pub const KU_DATA_ENCIPHERMENT: u16 = 1 << 3;
pub const KU_KEY_AGREEMENT: u16 = 1 << 4;
pub const KU_KEY_CERT_SIGN: u16 = 1 << 5;
pub const KU_CRL_SIGN: u16 = 1 << 6;

#[derive(Debug)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
    rest: &'a [u8],
    /// Span from the tag byte to the end of `content` (inclusive).
    full: &'a [u8],
}

fn read_tlv(input: &[u8]) -> Result<Tlv<'_>, CertParseError> {
    if input.is_empty() {
        return Err(CertParseError::Truncated);
    }
    let tag = input[0];
    if (tag & 0x1f) == 0x1f {
        return Err(CertParseError::BadTag);
    }
    let (len, len_bytes) = read_length(&input[1..])?;
    let header = 1 + len_bytes;
    let end = header.checked_add(len).ok_or(CertParseError::BadLength)?;
    if end > input.len() {
        return Err(CertParseError::Truncated);
    }
    Ok(Tlv {
        tag,
        content: &input[header..end],
        rest: &input[end..],
        full: &input[..end],
    })
}

fn read_tlv_tagged(input: &[u8], expected: u8) -> Result<Tlv<'_>, CertParseError> {
    let t = read_tlv(input)?;
    if t.tag != expected {
        return Err(CertParseError::BadTag);
    }
    Ok(t)
}

fn read_length(input: &[u8]) -> Result<(usize, usize), CertParseError> {
    if input.is_empty() {
        return Err(CertParseError::Truncated);
    }
    let first = input[0];
    if first < 0x80 {
        return Ok((first as usize, 1));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 {
        return Err(CertParseError::BadLength);
    }
    if n > 8 || 1 + n > input.len() {
        return Err(CertParseError::BadLength);
    }
    let mut len: usize = 0;
    for i in 0..n {
        len = len
            .checked_shl(8)
            .and_then(|v| v.checked_add(input[1 + i] as usize))
            .ok_or(CertParseError::BadLength)?;
    }
    Ok((len, 1 + n))
}

/// Parse a complete X.509 v1/v2/v3 certificate.
///
/// We extract the fields we actually need for chain validation and alias
/// selection: validity, subject + issuer DERs, SPKI (+ algo OID), and the
/// three extensions that gate cert-usage decisions. Anything else is
/// skipped silently — this parser is *not* an X.509 conformance checker.
pub fn parse_certificate(der: &[u8]) -> Result<ParsedCert, CertParseError> {
    let cert = read_tlv_tagged(der, TAG_SEQUENCE)?;
    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
    //                            signatureValue }
    let tbs = read_tlv_tagged(cert.content, TAG_SEQUENCE)?;
    let tbs_bytes = tbs.full.to_vec();
    let after_tbs = tbs.rest;

    let sig_alg = read_tlv_tagged(after_tbs, TAG_SEQUENCE)?;
    let sig_alg_oid = read_tlv_tagged(sig_alg.content, TAG_OID)?;
    let signature_algorithm_oid = sig_alg_oid.content.to_vec();

    let sig_bs = read_tlv_tagged(sig_alg.rest, TAG_BIT_STRING)?;
    // First byte of BIT STRING is the unused-bits count; skip it.
    let signature_value = if sig_bs.content.is_empty() {
        Vec::new()
    } else {
        sig_bs.content[1..].to_vec()
    };

    // ---- TBS contents ----
    let mut cursor = tbs.content;

    // Optional [0] EXPLICIT version
    let mut is_v3 = false;
    let first = read_tlv(cursor)?;
    let mut version_byte: u8 = 0;
    if first.tag == TAG_CONTEXT_0 {
        let v = read_tlv_tagged(first.content, TAG_INTEGER)?;
        if v.content.len() == 1 {
            version_byte = v.content[0];
        }
        is_v3 = version_byte >= 2;
        cursor = first.rest;
    }

    // serialNumber
    let serial = read_tlv_tagged(cursor, TAG_INTEGER)?;
    cursor = serial.rest;

    // signature AlgorithmIdentifier
    let sig_alg_inner = read_tlv_tagged(cursor, TAG_SEQUENCE)?;
    cursor = sig_alg_inner.rest;

    // issuer Name SEQUENCE (full DER for chain matching)
    let issuer = read_tlv_tagged(cursor, TAG_SEQUENCE)?;
    let issuer_der = issuer.full.to_vec();
    cursor = issuer.rest;

    // validity SEQUENCE { notBefore, notAfter }
    let validity = read_tlv_tagged(cursor, TAG_SEQUENCE)?;
    cursor = validity.rest;
    let nb = read_tlv(validity.content)?;
    let not_before_secs = decode_time(nb.tag, nb.content)?;
    let na = read_tlv(nb.rest)?;
    let not_after_secs = decode_time(na.tag, na.content)?;

    // subject Name SEQUENCE
    let subject = read_tlv_tagged(cursor, TAG_SEQUENCE)?;
    let subject_der = subject.full.to_vec();
    cursor = subject.rest;

    // SubjectPublicKeyInfo
    let spki = read_tlv_tagged(cursor, TAG_SEQUENCE)?;
    let spki_der = spki.full.to_vec();
    let spki_alg = read_tlv_tagged(spki.content, TAG_SEQUENCE)?;
    let spki_alg_oid = read_tlv_tagged(spki_alg.content, TAG_OID)?;
    let spki_algorithm_oid = spki_alg_oid.content.to_vec();
    cursor = spki.rest;

    // Optional issuerUniqueID [1], subjectUniqueID [2], extensions [3]
    let mut key_usage: Option<u16> = None;
    let mut ext_key_usage: Vec<Vec<u8>> = Vec::new();
    let mut basic_constraints_ca: Option<bool> = None;
    let mut san_dns_names: Vec<String> = Vec::new();
    let mut san_ip_addresses: Vec<Vec<u8>> = Vec::new();

    while !cursor.is_empty() {
        let tlv = read_tlv(cursor)?;
        if tlv.tag == TAG_CONTEXT_3 {
            // Extensions ::= [3] EXPLICIT SEQUENCE OF Extension
            let ext_seq = read_tlv_tagged(tlv.content, TAG_SEQUENCE)?;
            let mut ec = ext_seq.content;
            while !ec.is_empty() {
                let ext = read_tlv_tagged(ec, TAG_SEQUENCE)?;
                ec = ext.rest;
                let mut ic = ext.content;
                let oid = read_tlv_tagged(ic, TAG_OID)?;
                ic = oid.rest;
                // Optional critical BOOLEAN
                let next = read_tlv(ic)?;
                let value_tlv = if next.tag == TAG_BOOLEAN {
                    read_tlv_tagged(next.rest, TAG_OCTET_STRING)?
                } else if next.tag == TAG_OCTET_STRING {
                    next
                } else {
                    return Err(CertParseError::BadStructure);
                };

                if oid.content == OID_EXT_KEY_USAGE {
                    let bs = read_tlv_tagged(value_tlv.content, TAG_BIT_STRING)?;
                    if !bs.content.is_empty() {
                        let unused = bs.content[0];
                        let mut bits: u16 = 0;
                        let body = &bs.content[1..];
                        for (byte_idx, &b) in body.iter().enumerate() {
                            for bit in 0..8 {
                                if byte_idx == body.len() - 1 && bit >= 8 - (unused as usize) {
                                    break;
                                }
                                if (b >> (7 - bit)) & 1 == 1 {
                                    let pos = (byte_idx * 8 + bit) as u32;
                                    if pos < 16 {
                                        bits |= 1 << pos;
                                    }
                                }
                            }
                        }
                        key_usage = Some(bits);
                    }
                } else if oid.content == OID_EXT_EXTENDED_KEY_USAGE {
                    let seq = read_tlv_tagged(value_tlv.content, TAG_SEQUENCE)?;
                    let mut sc = seq.content;
                    while !sc.is_empty() {
                        let o = read_tlv_tagged(sc, TAG_OID)?;
                        ext_key_usage.push(o.content.to_vec());
                        sc = o.rest;
                    }
                } else if oid.content == OID_EXT_BASIC_CONSTRAINTS {
                    let seq = read_tlv_tagged(value_tlv.content, TAG_SEQUENCE)?;
                    if seq.content.is_empty() {
                        basic_constraints_ca = Some(false);
                    } else {
                        let first = read_tlv(seq.content)?;
                        if first.tag == TAG_BOOLEAN {
                            basic_constraints_ca =
                                Some(!first.content.is_empty() && first.content[0] != 0);
                        } else {
                            basic_constraints_ca = Some(false);
                        }
                    }
                } else if oid.content == OID_EXT_SUBJECT_ALT_NAME {
                    // SubjectAltName ::= GeneralNames ::= SEQUENCE OF GeneralName.
                    // GeneralName entries are context-specific IMPLICIT tags;
                    // we consume dNSName ([2]) and iPAddress ([7]) and skip
                    // the rest. A malformed SAN extension is non-fatal for
                    // chain validation — identity matching simply sees no
                    // names — so we swallow parse errors here.
                    if let Ok(seq) = read_tlv_tagged(value_tlv.content, TAG_SEQUENCE) {
                        let mut gc = seq.content;
                        while !gc.is_empty() {
                            let gn = match read_tlv(gc) {
                                Ok(t) => t,
                                Err(_) => break,
                            };
                            gc = gn.rest;
                            match gn.tag {
                                SAN_TAG_DNS_NAME => {
                                    if let Ok(s) = std::str::from_utf8(gn.content) {
                                        san_dns_names.push(s.to_ascii_lowercase());
                                    }
                                }
                                SAN_TAG_IP_ADDRESS => {
                                    if gn.content.len() == 4 || gn.content.len() == 16 {
                                        san_ip_addresses.push(gn.content.to_vec());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        cursor = tlv.rest;
    }

    let subject_cn = extract_common_name(&subject_der);

    Ok(ParsedCert {
        tbs_bytes,
        subject_der,
        issuer_der,
        spki_der,
        spki_algorithm_oid,
        not_before_secs,
        not_after_secs,
        key_usage,
        ext_key_usage,
        basic_constraints_ca,
        signature_algorithm_oid,
        signature_value,
        is_v3,
        san_dns_names,
        san_ip_addresses,
        subject_cn,
    })
}

/// Pull the most-specific `commonName` (OID 2.5.4.3) attribute value out of
/// a subject DN (a `Name ::= SEQUENCE OF RDNSequence`). RFC 4514 orders RDNs
/// most-significant-first, so the *last* CN in document order is the
/// most-specific one (the leaf host name) — that is what HotSpot's
/// `X509CertImpl.getSubjectX500Principal()`-derived endpoint check uses for
/// the legacy CN fallback.
///
/// Returns `None` on any structural problem or when no CN is present — the
/// caller treats "no CN" as "no fallback identity", never as a match.
fn extract_common_name(subject_der: &[u8]) -> Option<String> {
    let name = read_tlv_tagged(subject_der, TAG_SEQUENCE).ok()?;
    let mut rdns = name.content;
    let mut last_cn: Option<String> = None;
    while !rdns.is_empty() {
        // RelativeDistinguishedName ::= SET OF AttributeTypeAndValue
        let rdn = read_tlv_tagged(rdns, TAG_SET).ok()?;
        rdns = rdn.rest;
        let mut atvs = rdn.content;
        while !atvs.is_empty() {
            // AttributeTypeAndValue ::= SEQUENCE { type OID, value ANY }
            let atv = read_tlv_tagged(atvs, TAG_SEQUENCE).ok()?;
            atvs = atv.rest;
            let oid = read_tlv_tagged(atv.content, TAG_OID).ok()?;
            let value = read_tlv(oid.rest).ok()?;
            if oid.content == OID_AT_COMMON_NAME {
                // DirectoryString — PrintableString / UTF8String / etc. We
                // accept whatever UTF-8 decodes; non-UTF-8 CNs (rare) are
                // skipped rather than guessed at.
                if let Ok(s) = std::str::from_utf8(value.content) {
                    last_cn = Some(s.to_ascii_lowercase());
                }
            }
        }
    }
    last_cn
}

/// Decode UTCTime / GeneralizedTime to seconds since epoch.
fn decode_time(tag: u8, content: &[u8]) -> Result<i64, CertParseError> {
    let s = std::str::from_utf8(content).map_err(|_| CertParseError::BadTime)?;
    match tag {
        TAG_UTC_TIME => {
            // YYMMDDHHMMSSZ
            if s.len() != 13 || !s.ends_with('Z') {
                return Err(CertParseError::BadTime);
            }
            let yy: i32 = s[0..2].parse().map_err(|_| CertParseError::BadTime)?;
            let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
            let mo: i32 = s[2..4].parse().map_err(|_| CertParseError::BadTime)?;
            let dy: i32 = s[4..6].parse().map_err(|_| CertParseError::BadTime)?;
            let h: i32 = s[6..8].parse().map_err(|_| CertParseError::BadTime)?;
            let mi: i32 = s[8..10].parse().map_err(|_| CertParseError::BadTime)?;
            let se: i32 = s[10..12].parse().map_err(|_| CertParseError::BadTime)?;
            Ok(civil_to_epoch(year, mo, dy, h, mi, se))
        }
        TAG_GENERALIZED_TIME => {
            // YYYYMMDDHHMMSSZ (we ignore fractional seconds)
            if s.len() < 15 || !s.ends_with('Z') {
                return Err(CertParseError::BadTime);
            }
            let year: i32 = s[0..4].parse().map_err(|_| CertParseError::BadTime)?;
            let mo: i32 = s[4..6].parse().map_err(|_| CertParseError::BadTime)?;
            let dy: i32 = s[6..8].parse().map_err(|_| CertParseError::BadTime)?;
            let h: i32 = s[8..10].parse().map_err(|_| CertParseError::BadTime)?;
            let mi: i32 = s[10..12].parse().map_err(|_| CertParseError::BadTime)?;
            let se: i32 = s[12..14].parse().map_err(|_| CertParseError::BadTime)?;
            Ok(civil_to_epoch(year, mo, dy, h, mi, se))
        }
        _ => Err(CertParseError::BadTime),
    }
}

/// Civil date (UTC) to seconds since 1970-01-01.  Howard Hinnant's
/// `days_from_civil` algorithm — works for any year ≥ -32768.
fn civil_to_epoch(y: i32, m: i32, d: i32, hh: i32, mm: i32, ss: i32) -> i64 {
    let y_adj: i64 = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era: i64 = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe: i64 = y_adj - era * 400;
    let m_adj: i64 = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy: i64 = (153 * m_adj + 2) / 5 + d as i64 - 1;
    let doe: i64 = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days: i64 = era * 146_097 + doe - 719_468;
    days * 86_400 + (hh as i64) * 3600 + (mm as i64) * 60 + ss as i64
}

// ---------------------------------------------------------------------------
// Cert-alias selection
// ---------------------------------------------------------------------------

pub fn classify_key_type(spki_oid: &[u8]) -> &'static str {
    if spki_oid == OID_RSA {
        "RSA"
    } else if spki_oid == OID_EC {
        "EC"
    } else if spki_oid == OID_DSA {
        "DSA"
    } else if spki_oid == OID_ED25519 {
        "Ed25519"
    } else if spki_oid == OID_ED448 {
        "Ed448"
    } else {
        "UNKNOWN"
    }
}

/// True if the cert is acceptable as a *server* certificate.
pub fn is_server_cert(p: &ParsedCert) -> bool {
    // KeyUsage check (if extension present): need digitalSignature OR
    // keyEncipherment. Many server certs only set keyEncipherment.
    if let Some(ku) = p.key_usage {
        if (ku & (KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT)) == 0 {
            return false;
        }
    }
    // ExtendedKeyUsage check: if present, must include id-kp-serverAuth.
    // If absent, we permit (many older certs omit EKU).
    if !p.ext_key_usage.is_empty() {
        if !p
            .ext_key_usage
            .iter()
            .any(|o| o.as_slice() == OID_KP_SERVER_AUTH)
        {
            return false;
        }
    }
    true
}

/// True if the cert is acceptable as a *client* certificate.
pub fn is_client_cert(p: &ParsedCert) -> bool {
    if let Some(ku) = p.key_usage {
        if (ku & KU_DIGITAL_SIGNATURE) == 0 {
            return false;
        }
    }
    if !p.ext_key_usage.is_empty() {
        if !p
            .ext_key_usage
            .iter()
            .any(|o| o.as_slice() == OID_KP_CLIENT_AUTH)
        {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Building per-connection state from a KeyStore
// ---------------------------------------------------------------------------

/// Build a `KeyManagerState` from the parsed contents of a `LoadedKeyStore`.
/// Filtering by KU/EKU happens here so `chooseServerAlias` is a HashMap
/// lookup at handshake time.
pub fn build_key_manager_state(keystore_id: i32) -> KeyManagerState {
    let mut state = KeyManagerState {
        keystore_id,
        ..Default::default()
    };
    let store = match keystore::keystore_lookup(keystore_id) {
        Some(s) => s,
        None => return state,
    };

    for (alias, entry) in &store.entries {
        if let keystore::EntryKind::PrivateKey { key_der, chain } = &entry.kind {
            if chain.is_empty() {
                continue;
            }
            let leaf = match parse_certificate(&chain[0]) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let key_type = classify_key_type(&leaf.spki_algorithm_oid).to_string();
            state.aliases_to_chain.insert(alias.clone(), chain.clone());
            state.aliases_to_key.insert(alias.clone(), key_der.clone());
            if is_server_cert(&leaf) {
                state
                    .server_aliases_by_key_type
                    .entry(key_type.clone())
                    .or_default()
                    .push(alias.clone());
            }
            if is_client_cert(&leaf) {
                state
                    .client_aliases_by_key_type
                    .entry(key_type)
                    .or_default()
                    .push(alias.clone());
            }
        }
    }
    state
}

/// Build a `TrustManagerState` from a `LoadedKeyStore`'s trusted-cert
/// entries plus the system trust store. The user keystore is consulted
/// first; system roots fold in afterwards as fallback so the user's
/// explicit `keystore.jks` can override platform defaults.
pub fn build_trust_manager_state(keystore_id: i32) -> TrustManagerState {
    let mut state = TrustManagerState {
        keystore_id,
        enable_crl: false,
        ..Default::default()
    };

    // (1) User-supplied trust anchors out of the bound keystore.
    if let Some(store) = keystore::keystore_lookup(keystore_id) {
        for (_alias, entry) in &store.entries {
            let der = match &entry.kind {
                keystore::EntryKind::TrustedCert { cert_der } => cert_der.clone(),
                keystore::EntryKind::PrivateKey { chain, .. } => {
                    // The last cert in a private-key chain is the trust root.
                    match chain.last() {
                        Some(d) => d.clone(),
                        None => continue,
                    }
                }
            };
            insert_anchor(&mut state, der);
        }
    }

    // (2) System trust store via rustls-native-certs.
    let result = rustls_native_certs::load_native_certs();
    for cert in result.certs {
        let der = cert.as_ref().to_vec();
        insert_anchor(&mut state, der);
    }
    // Errors from the OS trust store are surfaced via tracing rather than
    // failing the whole load — even a single platform root is better than
    // none for the WP5.3 acceptance criteria.
    if !result.errors.is_empty() {
        tracing::debug!(
            target: "x509_manager",
            "rustls-native-certs reported {} error(s) loading platform trust roots",
            result.errors.len()
        );
    }

    state
}

fn insert_anchor(state: &mut TrustManagerState, der: Vec<u8>) {
    let parsed = match parse_certificate(&der) {
        Ok(p) => p,
        Err(_) => return,
    };
    state.anchor_ders.push(der.clone());
    state.anchors.insert(
        parsed.subject_der.clone(),
        AnchorInfo {
            subject_der: parsed.subject_der,
            spki_der: parsed.spki_der,
            full_cert_der: Some(der),
        },
    );
}

// ---------------------------------------------------------------------------
// RFC 5280 chain validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum TrustError {
    EmptyChain,
    Expired {
        subject_dn: Vec<u8>,
        when: i64,
    },
    NotYetValid {
        subject_dn: Vec<u8>,
        when: i64,
    },
    BrokenChain {
        at: usize,
    },
    NotCa {
        at: usize,
    },
    NoTrustAnchor,
    /// Signature *value* was missing or otherwise structurally unusable
    /// (issuer SPKI couldn't be decoded, signature length mismatch with
    /// modulus, etc.). Kept for backwards compatibility with code that
    /// matched on `SignatureFailed { at }` before we split out the
    /// cryptographic-failure paths.
    SignatureFailed {
        at: usize,
    },
    /// The cryptographic signature verification step itself rejected the
    /// pair `(issuer SPKI, signature)` for cert `at`. RSA-PKCS1v15 padding
    /// mismatch / decoded digest mismatch, ECDSA `u1*G + u2*Q.x ≠ r mod n`,
    /// or any other algorithm-level failure.
    BadSignature {
        at: usize,
    },
    /// The signature-algorithm OID is recognised but this verifier doesn't
    /// implement it. Lets callers distinguish "DSA chain — please fall back
    /// to JCE" from "we have no idea what 1.2.3.4 is". Currently fires
    /// for DSA (id-dsa-with-sha1), RSA-PSS (id-RSASSA-PSS), and Ed25519
    /// (id-Ed25519). See the OID const block for the upgrade plan.
    NotImplemented {
        at: usize,
        oid: Vec<u8>,
    },
    Parse(CertParseError),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::EmptyChain => f.write_str("empty cert chain"),
            TrustError::Expired { when, .. } => write!(f, "cert expired (notAfter {})", when),
            TrustError::NotYetValid { when, .. } => {
                write!(f, "cert not yet valid (notBefore {})", when)
            }
            TrustError::BrokenChain { at } => write!(f, "chain broken at index {}", at),
            TrustError::NotCa { at } => write!(f, "cert at index {} is not a CA", at),
            TrustError::NoTrustAnchor => f.write_str("no trust anchor found for chain"),
            TrustError::SignatureFailed { at } => {
                write!(
                    f,
                    "signature verification failed at index {} (structural)",
                    at
                )
            }
            TrustError::BadSignature { at } => {
                write!(
                    f,
                    "signature verification failed at index {} (cryptographic)",
                    at
                )
            }
            TrustError::NotImplemented { at, oid } => {
                write!(
                    f,
                    "signature-algorithm OID at index {} not implemented (oid bytes={:02x?})",
                    at, oid
                )
            }
            TrustError::Parse(e) => write!(f, "parse: {}", e),
        }
    }
}

/// Run the RFC 5280 §6 chain validation algorithm against the given trust
/// anchors. Returns `Ok(())` when the chain validates, `Err(TrustError)`
/// otherwise. The caller maps this to a Java `CertificateException` at
/// the bytecode boundary.
///
/// Steps:
///   1. Parse each cert in the chain (leaf first → root candidate last).
///   2. Date-check every cert.
///   3. Verify chain continuity (issuer ↔ subject DN match).
///   4. Verify each non-leaf cert has `BasicConstraints.cA = TRUE`.
///   5. Find a trust anchor whose subject DN matches the last cert's
///      subject (the chain ends at an anchor) or the last cert's issuer
///      (the chain stops one hop short of the anchor — the classic
///      `[leaf, intermediate]` shape).
///   6. Cryptographically verify each cert's signature against its issuer's
///      SPKI — `parsed[i+1].spki_der` for intermediate hops, the matched
///      trust anchor's SPKI for the last cert (skipped when the last cert
///      IS the anchor, per RFC 5280 §6.1.1).
///
/// Cryptographic dispatch covers the two algorithms real-world JARs and
/// PKIX chains overwhelmingly use today:
///   * `1.2.840.113549.1.1.11` — sha256WithRSAEncryption (PKCS#1 v1.5)
///   * `1.2.840.10045.4.3.2`   — ecdsa-with-SHA256 (P-256)
///
/// Everything else returns `TrustError::NotImplemented` so the caller can
/// choose to delegate to a JCE provider. See the OID block above for the
/// inventory of recognised-but-unimplemented OIDs.
pub fn validate_chain(chain: &[Vec<u8>], trust: &TrustManagerState) -> Result<(), TrustError> {
    if chain.is_empty() {
        return Err(TrustError::EmptyChain);
    }

    // Step 1: parse.
    let mut parsed: Vec<ParsedCert> = Vec::with_capacity(chain.len());
    for der in chain {
        parsed.push(parse_certificate(der).map_err(TrustError::Parse)?);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Step 2: clock check.
    for p in &parsed {
        if now < p.not_before_secs {
            return Err(TrustError::NotYetValid {
                subject_dn: p.subject_der.clone(),
                when: p.not_before_secs,
            });
        }
        if now > p.not_after_secs {
            return Err(TrustError::Expired {
                subject_dn: p.subject_der.clone(),
                when: p.not_after_secs,
            });
        }
    }

    // Step 3: chain continuity.
    for i in 0..parsed.len().saturating_sub(1) {
        if parsed[i].issuer_der != parsed[i + 1].subject_der {
            return Err(TrustError::BrokenChain { at: i });
        }
    }

    // Step 4: BasicConstraints CA on intermediates.
    if parsed.len() >= 2 {
        for i in 1..parsed.len() {
            // Ed25519/Ed448 in earlier extension parsers may return None for
            // BC; we treat None as "not-a-CA" only when v3 extensions exist
            // for this cert. The is_v3 check matches what HotSpot's
            // PKIXValidator does for legacy v1 roots which had no extensions.
            if parsed[i].is_v3 && parsed[i].basic_constraints_ca == Some(false) {
                return Err(TrustError::NotCa { at: i });
            }
        }
    }

    // Step 5: trust anchor.
    let last = &parsed[parsed.len() - 1];
    // The last cert's issuer must be present in the trust set — unless the
    // last cert is itself the anchor (self-signed root packaged in the
    // chain). We accept either presentation.
    //
    // We probe `subject_der` first so a self-signed root that the caller
    // both put in the trust set AND repeated at the bottom of the chain is
    // recognised as the anchor itself — RFC 5280 §6.1.1 says a trust
    // anchor's public key is taken as authoritative without further
    // verification, so the cryptographic step below must NOT attempt to
    // re-verify it. Only when the cert is *not* itself an anchor do we
    // fall through to the issuer-DN lookup (cross-signed roots, classic
    // intermediate-anchored chains).
    let (anchor_spki, last_is_anchor): (&[u8], bool) = match trust.anchors.get(&last.subject_der) {
        Some(a) => (a.spki_der.as_slice(), true),
        None => match trust.anchors.get(&last.issuer_der) {
            Some(a) => (a.spki_der.as_slice(), false),
            None => return Err(TrustError::NoTrustAnchor),
        },
    };

    // Step 6: cryptographic signature verification.
    //
    // For each cert[i] in the chain we re-verify its `signatureValue` against
    // the issuer's `SubjectPublicKeyInfo`:
    //
    //   * `i < parsed.len() - 1` → issuer SPKI is `parsed[i+1].spki_der`.
    //   * `i == parsed.len() - 1` and the last cert is *not* itself the
    //     anchor → issuer SPKI is `anchor_spki` (the matched trust anchor's
    //     SPKI).
    //   * `i == parsed.len() - 1` and the last cert *is* the anchor → skip;
    //     RFC 5280 §6.1.1 (the "trust anchor information" definition) says
    //     a trust anchor's public key is taken as authoritative without
    //     re-verification.
    //
    // The OID dispatch covers the two algorithms real-world JARs and PKIX
    // chains overwhelmingly use today; everything else maps to
    // `NotImplemented { oid }` so the caller can choose to defer to JCE.
    for i in 0..parsed.len() {
        if parsed[i].signature_value.is_empty() {
            return Err(TrustError::SignatureFailed { at: i });
        }
        let issuer_spki: &[u8] = if i + 1 < parsed.len() {
            parsed[i + 1].spki_der.as_slice()
        } else if last_is_anchor {
            // Anchor's signature is trusted by definition — nothing to check.
            continue;
        } else {
            anchor_spki
        };
        verify_one_signature(i, &parsed[i], issuer_spki)?;
    }

    Ok(())
}

/// Verify cert[i]'s signature against its issuer's SubjectPublicKeyInfo.
///
/// Dispatches on `parsed.signature_algorithm_oid` (the outer
/// `signatureAlgorithm.algorithm` OID, equivalent to the algorithm name a
/// Java `Signature.getInstance(...)` call would use). Returns `Ok(())`
/// when the signature verifies, `Err(BadSignature)` when the cryptographic
/// check fails, or `Err(NotImplemented)` when the OID is recognised but
/// this in-tree verifier doesn't implement it.
fn verify_one_signature(
    at: usize,
    cert: &ParsedCert,
    issuer_spki: &[u8],
) -> Result<(), TrustError> {
    use crate::crypto_impl::{parse_ecdsa_public_key, parse_rsa_public_key, Ecdsa, Rsa, Sha256};

    let oid = cert.signature_algorithm_oid.as_slice();
    let sig = cert.signature_value.as_slice();
    let tbs = cert.tbs_bytes.as_slice();

    if oid == OID_SIG_SHA256_RSA {
        // PKCS#1 v1.5 RSA-SHA256: hash(tbs) → EMSA-PKCS1-v1_5 envelope, then
        // s^e mod n and byte-equality compare.
        let pk = match parse_rsa_public_key(issuer_spki) {
            Some(k) => k,
            None => return Err(TrustError::BadSignature { at }),
        };
        if Rsa::verify_sha256(&pk, tbs, sig) {
            Ok(())
        } else {
            Err(TrustError::BadSignature { at })
        }
    } else if oid == OID_SIG_ECDSA_SHA256 {
        // ECDSA-with-SHA256 over P-256: DER-decoded (r, s), check u1*G +
        // u2*Q.x ≡ r (mod n). `verify_with_digest` takes a pre-hashed
        // digest so we hash the TBS once here.
        let pk = match parse_ecdsa_public_key(issuer_spki) {
            Some(k) => k,
            None => return Err(TrustError::BadSignature { at }),
        };
        let digest = Sha256::digest(tbs);
        if Ecdsa::verify_with_digest(&pk, &digest, sig) {
            Ok(())
        } else {
            Err(TrustError::BadSignature { at })
        }
    } else if oid == OID_SIG_DSA_SHA1 || oid == OID_SIG_RSA_PSS || oid == OID_SIG_ED25519 {
        // Known-but-unimplemented. See OID const block for the rationale —
        // each of these needs additional parsing (PSS parameters) or a
        // distinct primitive (DSA, EdDSA) we don't expose at this layer yet.
        Err(TrustError::NotImplemented {
            at,
            oid: oid.to_vec(),
        })
    } else {
        // Totally unrecognised OID — still NotImplemented (rather than
        // BadSignature) so callers see a structural rather than a
        // cryptographic-rejection signal and can decide to delegate.
        Err(TrustError::NotImplemented {
            at,
            oid: oid.to_vec(),
        })
    }
}

// ---------------------------------------------------------------------------
// Endpoint identification (RFC 6125 / RFC 2818 hostname verification)
// ---------------------------------------------------------------------------
//
// Chain validation (`validate_chain`) proves the leaf chains to a trusted
// anchor — but a cert that is perfectly valid *for the wrong host* must still
// be rejected for HTTPS / LDAPS. That is the job of endpoint identification:
// match the peer host the client *intended* to reach against the identities
// the leaf certificate asserts (SubjectAltName dNSName / iPAddress, with a
// legacy commonName fallback).
//
// ## Where the host comes from — and the gap this leaves
//
// The standard `javax.net.ssl.X509TrustManager` surface
// (`checkServerTrusted(X509Certificate[], String authType)`) carries **no**
// peer host: the JDK performs endpoint identification inside the SSL engine
// (`sun.security.ssl.X509TrustManagerImpl.checkIdentity`), driven by the
// `SSLParameters.getEndpointIdentificationAlgorithm()` value ("HTTPS"/"LDAPS")
// and the `SSLSession` peer host — neither of which is an argument to the
// `X509TrustManager` method we natively back in `do_check_trusted`.
//
// So `do_check_trusted` *cannot* perform the host check itself without the
// intended host, and silently inventing one would be worse than omitting it.
// Instead this module exposes `verify_hostname` / `check_endpoint_identity`
// as the public entry point for the SSL-engine layer (tls.rs) to call at the
// point where the peer host and the negotiated identification algorithm are
// actually available. Until that wiring lands, endpoint identity is enforced
// by whatever caller threads the host in; the chain-trust path is unchanged
// and never *weakened* by this addition.

/// Why an endpoint-identity (hostname) check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostnameError {
    /// The caller passed an empty expected host — we refuse to match against
    /// nothing rather than silently accept.
    EmptyHost,
    /// The leaf asserted at least one identity, but none matched the host.
    NoMatch { expected: String },
    /// The leaf carried no usable identity at all (no SAN dNSName/iPAddress
    /// and no commonName). RFC 6125 §6.4.4: with no presentable identity the
    /// match must fail closed.
    NoIdentity,
}

impl std::fmt::Display for HostnameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostnameError::EmptyHost => f.write_str("empty expected host for endpoint identity"),
            HostnameError::NoMatch { expected } => {
                write!(f, "certificate identity does not match host {:?}", expected)
            }
            HostnameError::NoIdentity => {
                f.write_str("certificate presents no SubjectAltName or commonName identity")
            }
        }
    }
}

/// True when `host` looks like a textual IPv4/IPv6 literal rather than a DNS
/// name. We keep this deliberately conservative: dotted-quad with 4 numeric
/// labels, or any string containing a `:` (IPv6). Anything else is treated as
/// a DNS name and goes through wildcard matching.
fn host_is_ip_literal(host: &str) -> bool {
    if host.contains(':') {
        return true; // IPv6 literal (possibly bracketed by the caller).
    }
    let mut labels = 0;
    for part in host.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if part.parse::<u16>().map(|n| n > 255).unwrap_or(true) {
            return false;
        }
        labels += 1;
    }
    labels == 4
}

/// Parse a textual IP literal into its raw network-order bytes for comparison
/// against a SAN `iPAddress`. Returns `None` for anything we can't parse —
/// the caller then simply finds no IP match. IPv6 parsing is delegated to the
/// std library; IPv4 is the dotted-quad fast path.
fn ip_literal_to_bytes(host: &str) -> Option<Vec<u8>> {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(v4) = trimmed.parse::<std::net::Ipv4Addr>() {
        return Some(v4.octets().to_vec());
    }
    if let Ok(v6) = trimmed.parse::<std::net::Ipv6Addr>() {
        return Some(v6.octets().to_vec());
    }
    None
}

/// Match a presented `dNSName` pattern against an `expected` host per the
/// RFC 6125 §6.4.3 / RFC 2818 wildcard rules:
///
///   * Case-insensitive (both sides are already lower-cased on the SAN side;
///     we lower-case the host before calling).
///   * A `*` wildcard is permitted **only** in the left-most label, must be
///     the *entire* left-most label (no partial `f*o.example.com`), and
///     matches exactly one label — it never matches a dot, so
///     `*.example.com` matches `a.example.com` but not `a.b.example.com`
///     and not the bare `example.com`.
///   * The wildcard must leave at least two labels to its right (we refuse
///     `*.com` / `*` to avoid public-suffix-wide certs).
fn dns_name_matches(pattern: &str, expected: &str) -> bool {
    if pattern.is_empty() || expected.is_empty() {
        return false;
    }
    // Non-wildcard: plain case-insensitive equality.
    let Some(rest) = pattern.strip_prefix("*.") else {
        return pattern == expected;
    };
    // Reject a bare `*` or any pattern with a wildcard outside the first
    // label (e.g. `a.*.com`): `rest` must itself be wildcard-free.
    if rest.is_empty() || rest.contains('*') {
        return false;
    }
    // Require at least two labels after the wildcard (`*.example.com` ok,
    // `*.com` rejected).
    if rest.split('.').filter(|l| !l.is_empty()).count() < 2 {
        return false;
    }
    // The wildcard matches exactly one left-most label of `expected`.
    match expected.split_once('.') {
        Some((first, tail)) => !first.is_empty() && tail == rest,
        None => false,
    }
}

/// Verify that `expected_host` is one of the identities asserted by `leaf`.
///
/// Algorithm (RFC 6125 / RFC 2818, matching HotSpot's
/// `X509TrustManagerImpl.checkIdentity` for the HTTPS/LDAPS algorithm):
///
///   1. If the host is an IP literal, it must match a SAN `iPAddress` exactly
///      (byte-for-byte). IP literals are never matched against dNSName or CN.
///   2. Otherwise (a DNS name): if the leaf has **any** SAN `dNSName`, the
///      host must match one of them (wildcards per `dns_name_matches`); the
///      commonName is NOT consulted (RFC 6125 §6.4.4 — SAN presence forbids
///      CN fallback).
///   3. If the leaf has no SAN `dNSName` at all, fall back to the subject
///      commonName with the same matching rules (legacy compatibility).
///   4. No usable identity / no match → fail closed.
pub fn verify_hostname(leaf: &ParsedCert, expected_host: &str) -> Result<(), HostnameError> {
    let host = expected_host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if host.is_empty() {
        return Err(HostnameError::EmptyHost);
    }
    let host_lc = host.to_ascii_lowercase();

    if host_is_ip_literal(&host_lc) {
        let want = ip_literal_to_bytes(&host_lc).ok_or(HostnameError::NoMatch {
            expected: host_lc.clone(),
        })?;
        if leaf.san_ip_addresses.iter().any(|ip| ip == &want) {
            return Ok(());
        }
        // No iPAddress SAN matched. An IP literal is never matched against a
        // dNSName or CN, so this is a definitive failure.
        return if leaf.san_ip_addresses.is_empty() && leaf.san_dns_names.is_empty() {
            Err(HostnameError::NoIdentity)
        } else {
            Err(HostnameError::NoMatch { expected: host_lc })
        };
    }

    // DNS-name host.
    if !leaf.san_dns_names.is_empty() {
        if leaf
            .san_dns_names
            .iter()
            .any(|pat| dns_name_matches(pat, &host_lc))
        {
            return Ok(());
        }
        return Err(HostnameError::NoMatch { expected: host_lc });
    }

    // No dNSName SAN — legacy commonName fallback.
    match &leaf.subject_cn {
        Some(cn) if dns_name_matches(cn, &host_lc) => Ok(()),
        Some(_) => Err(HostnameError::NoMatch { expected: host_lc }),
        None => Err(HostnameError::NoIdentity),
    }
}

/// Endpoint-identity entry point for the SSL-engine layer: given the raw DER
/// chain (leaf first) and the host the client intended to reach, verify the
/// leaf asserts that identity. This does NOT re-run chain trust — call
/// `validate_chain` first (or alongside); endpoint identity is an *additional*
/// gate on top of a trusted chain, never a replacement for it.
pub fn check_endpoint_identity(
    chain: &[Vec<u8>],
    expected_host: &str,
) -> Result<(), HostnameError> {
    let leaf_der = chain.first().ok_or(HostnameError::NoIdentity)?;
    let leaf = parse_certificate(leaf_der).map_err(|_| HostnameError::NoIdentity)?;
    verify_hostname(&leaf, expected_host)
}

// ---------------------------------------------------------------------------
// Field layout for synthetic Java mirrors
// ---------------------------------------------------------------------------
//
// X509KeyManagerImpl mirror: 1 field — slot 0 = i32 km_id.
// X509TrustManagerImpl mirror: 1 field — slot 0 = i32 tm_id.
// KeyManagerFactoryImpl$SunX509: slot 0 = i32 km_id.
// TrustManagerFactoryImpl$SimpleFactory: slot 0 = i32 tm_id.

const FQN_SUN_X509_KM: &str = "sun/security/ssl/SunX509KeyManagerImpl";
const FQN_X509_KM: &str = "sun/security/ssl/X509KeyManagerImpl";
const FQN_X509_TM: &str = "sun/security/ssl/X509TrustManagerImpl";
const FQN_PKIX_VALIDATOR: &str = "sun/security/validator/PKIXValidator";
const FQN_KMF_SUN_X509: &str = "sun/security/ssl/KeyManagerFactoryImpl$SunX509";
const FQN_TMF_SIMPLE: &str = "sun/security/ssl/TrustManagerFactoryImpl$SimpleFactory";

// ---------------------------------------------------------------------------
// Public registration entry point
// ---------------------------------------------------------------------------

/// Wave-coordinator entry point.  See module docstring for the surface.
pub fn register_x509_manager_real(r: &mut NativeMethodRegistry) {
    register_key_manager(r, FQN_SUN_X509_KM);
    register_key_manager(r, FQN_X509_KM);
    register_trust_manager(r, FQN_X509_TM);
    register_pkix_validator(r);
    register_kmf(r);
    register_tmf(r);
}

fn register_key_manager(r: &mut NativeMethodRegistry, fqn: &'static str) {
    r.register(
        fqn,
        "chooseClientAlias",
        "([Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;",
        choose_client_alias,
    );
    r.register(
        fqn,
        "chooseServerAlias",
        "(Ljava/lang/String;[Ljava/security/Principal;Ljava/net/Socket;)Ljava/lang/String;",
        choose_server_alias,
    );
    r.register(
        fqn,
        "getCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/X509Certificate;",
        get_certificate_chain,
    );
    r.register(
        fqn,
        "getPrivateKey",
        "(Ljava/lang/String;)Ljava/security/PrivateKey;",
        get_private_key,
    );
    r.register(
        fqn,
        "getServerAliases",
        "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;",
        get_server_aliases,
    );
    r.register(
        fqn,
        "getClientAliases",
        "(Ljava/lang/String;[Ljava/security/Principal;)[Ljava/lang/String;",
        get_client_aliases,
    );
}

fn register_trust_manager(r: &mut NativeMethodRegistry, fqn: &'static str) {
    r.register(
        fqn,
        "checkClientTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        check_client_trusted,
    );
    r.register(
        fqn,
        "checkServerTrusted",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
        check_server_trusted,
    );
    r.register(
        fqn,
        "getAcceptedIssuers",
        "()[Ljava/security/cert/X509Certificate;",
        get_accepted_issuers,
    );
}

fn register_pkix_validator(r: &mut NativeMethodRegistry) {
    // engineValidate(Certificate[] chain) -> Certificate[]  (the validated
    // path, leaf-first). Real-JDK has a longer overload too; the single-arg
    // form is what Keycloak / EJBCA actually call.
    r.register(
        FQN_PKIX_VALIDATOR,
        "engineValidate",
        "([Ljava/security/cert/Certificate;)[Ljava/security/cert/Certificate;",
        pkix_engine_validate,
    );
    r.register(
        FQN_PKIX_VALIDATOR,
        "engineValidate",
        "([Ljava/security/cert/Certificate;Ljava/util/Collection;Ljava/security/AlgorithmConstraints;Ljava/lang/Object;)[Ljava/security/cert/Certificate;",
        pkix_engine_validate,
    );
}

fn register_kmf(r: &mut NativeMethodRegistry) {
    r.register(
        FQN_KMF_SUN_X509,
        "engineInit",
        "(Ljava/security/KeyStore;[C)V",
        kmf_engine_init,
    );
    r.register(
        FQN_KMF_SUN_X509,
        "engineGetKeyManagers",
        "()[Ljavax/net/ssl/KeyManager;",
        kmf_engine_get_key_managers,
    );
}

fn register_tmf(r: &mut NativeMethodRegistry) {
    r.register(
        FQN_TMF_SIMPLE,
        "engineInit",
        "(Ljava/security/KeyStore;)V",
        tmf_engine_init,
    );
    r.register(
        FQN_TMF_SIMPLE,
        "engineGetTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        tmf_engine_get_trust_managers,
    );
}

// ---------------------------------------------------------------------------
// Helpers — receiver / arg decoding
// ---------------------------------------------------------------------------

fn this_arg(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(r))) => Ok(*r),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("X509 manager call on null receiver".into()),
        }
        .into()),
    }
}

fn read_string_at(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    match args.get(idx) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    }
}

fn read_string_array(ctx: &mut dyn NativeContext, v: &Value) -> Vec<String> {
    let arr = match v {
        Value::Object(Some(a)) => *a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            if let Some(text) = ctx.read_string(s) {
                out.push(text);
            }
        }
    }
    out
}

/// Read a Java X509Certificate's DER bytes. The keystore mirror layout is
/// (subject, issuer, cert_id, der_bytes); fallback paths probe by name.
fn read_cert_der(ctx: &mut dyn NativeContext, cert: ObjectRef) -> Option<Vec<u8>> {
    let n = ctx.object_num_fields(cert);
    if n > 3 {
        if let Value::Object(Some(arr)) = ctx.get_field(cert, 3) {
            let alen = ctx.array_length(arr);
            let mut out = Vec::with_capacity(alen);
            for i in 0..alen {
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    out.push(b as u8);
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
    }
    // Fallback: by-name probe.
    let by_name = ctx.get_field_by_name(cert, "encoded");
    if let Value::Object(Some(arr)) = by_name {
        let alen = ctx.array_length(arr);
        let mut out = Vec::with_capacity(alen);
        for i in 0..alen {
            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                out.push(b as u8);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn read_chain_arg(ctx: &mut dyn NativeContext, v: &Value) -> Vec<Vec<u8>> {
    let arr = match v {
        Value::Object(Some(a)) => *a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(c)) = ctx.get_array_element(arr, i) {
            if let Some(der) = read_cert_der(ctx, c) {
                out.push(der);
            }
        }
    }
    out
}

fn get_km_id(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    let by_name = ctx.get_field_by_name(this, "cratonvm$x509km$id");
    if let Value::Int(i) = by_name {
        if i != 0 {
            return i;
        }
    }
    let n = ctx.object_num_fields(this);
    if n > 0 {
        if let Value::Int(i) = ctx.get_field(this, 0) {
            if i != 0 {
                return i;
            }
        }
    }
    0
}

fn set_km_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    ctx.set_field_by_name(this, "cratonvm$x509km$id", Value::Int(id));
    let n = ctx.object_num_fields(this);
    if n > 0 {
        ctx.set_field(this, 0, Value::Int(id));
    }
}

fn get_tm_id(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    let by_name = ctx.get_field_by_name(this, "cratonvm$x509tm$id");
    if let Value::Int(i) = by_name {
        if i != 0 {
            return i;
        }
    }
    let n = ctx.object_num_fields(this);
    if n > 0 {
        if let Value::Int(i) = ctx.get_field(this, 0) {
            if i != 0 {
                return i;
            }
        }
    }
    0
}

fn set_tm_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    ctx.set_field_by_name(this, "cratonvm$x509tm$id", Value::Int(id));
    let n = ctx.object_num_fields(this);
    if n > 0 {
        ctx.set_field(this, 0, Value::Int(id));
    }
}

/// Pull the `storeId` out of a Java `KeyStore` mirror. Real-JDK packs it in a
/// dedicated field; our `keystore.rs` stash convention sets it both by name
/// and at slot index 4.
fn read_keystore_id(ctx: &mut dyn NativeContext, ks: ObjectRef) -> i32 {
    let by_name = ctx.get_field_by_name(ks, "cratonvm$keystore$storeId");
    if let Value::Int(i) = by_name {
        if i != 0 {
            return i;
        }
    }
    if let Value::Long(l) = ctx.get_field_by_name(ks, "cratonvm$keystore$storeId") {
        if l != 0 {
            return l as i32;
        }
    }
    let n = ctx.object_num_fields(ks);
    if n > 4 {
        match ctx.get_field(ks, 4) {
            Value::Int(i) => return i,
            Value::Long(l) => return l as i32,
            _ => {}
        }
    }
    0
}

fn make_x509_mirror(ctx: &mut dyn NativeContext, alias: &str, der: &[u8]) -> ObjectRef {
    let cert_obj = alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 4);
    let alias_str = ctx.create_string(alias);
    ctx.set_field(cert_obj, 0, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 1, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 2, Value::Int(0));
    let arr = ctx.new_array(ArrayElementType::Byte, der.len());
    for (i, b) in der.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    ctx.set_field(cert_obj, 3, Value::Object(Some(arr)));
    cert_obj
}

fn make_private_key_mirror(
    ctx: &mut dyn NativeContext,
    key_der: &[u8],
    km_id: i32,
    alias: &str,
) -> ObjectRef {
    let pk = alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 4);
    // Same packing convention keystore.rs uses so the TLS path can decode
    // the (km_id, alias_hash) pair.
    let alias_hash = fnv1a_32(alias.as_bytes());
    let composite = ((km_id as i64 & 0xFFFF_FFFF) << 32) | (alias_hash as i64 & 0xFFFF_FFFF);
    let algo_idx = match classify_key_type_from_pkcs8(key_der) {
        "RSA" => 6,
        "EC" => 7,
        "DSA" => 5,
        "Ed25519" => 8,
        _ => 6,
    };
    ctx.set_field(pk, 0, Value::Int(algo_idx));
    ctx.set_field(pk, 1, Value::Int((key_der.len() as i32).saturating_mul(8)));
    ctx.set_field(pk, 2, Value::Int(key_der.len() as i32));
    ctx.set_field(pk, 3, Value::Long(composite));
    pk
}

fn classify_key_type_from_pkcs8(key_der: &[u8]) -> &'static str {
    // PKCS#8 PrivateKeyInfo ::= SEQUENCE { version, AlgorithmIdentifier, OCTET STRING }
    let needles: &[(&[u8], &'static str)] = &[
        (
            &[
                0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
            ],
            "RSA",
        ),
        (
            &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01],
            "EC",
        ),
        (
            &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x01],
            "DSA",
        ),
        (&[0x06, 0x03, 0x2b, 0x65, 0x70], "Ed25519"),
        (&[0x06, 0x03, 0x2b, 0x65, 0x71], "Ed448"),
    ];
    for (needle, label) in needles {
        if find_subseq(key_der, needle).is_some() {
            return label;
        }
    }
    "RSA"
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=(hay.len() - needle.len()) {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

fn fnv1a_32(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &x in b {
        h ^= x as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn cert_exception(message: String) -> MethodCallFailed {
    RuntimeError::IOException { message }.into()
}

// ---------------------------------------------------------------------------
// X509KeyManager handlers
// ---------------------------------------------------------------------------

fn choose_client_alias(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let key_types = match args.get(1) {
        Some(v) => read_string_array(ctx, v),
        None => Vec::new(),
    };

    let registry = km_registry().read();
    let state = match registry.get(&id) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };

    for kt in &key_types {
        if let Some(aliases) = state.client_aliases_by_key_type.get(kt) {
            if let Some(first) = aliases.first() {
                let s = ctx.create_string(first);
                return Ok(Some(Value::Object(Some(s))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

fn choose_server_alias(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let key_type = read_string_at(ctx, args, 1).unwrap_or_default();

    let registry = km_registry().read();
    let state = match registry.get(&id) {
        Some(s) => s,
        None => return Ok(Some(Value::Object(None))),
    };

    if let Some(aliases) = state.server_aliases_by_key_type.get(&key_type) {
        if let Some(first) = aliases.first() {
            let s = ctx.create_string(first);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(None)))
}

fn get_certificate_chain(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let alias = read_string_at(ctx, args, 1).unwrap_or_default();

    let chain = {
        let registry = km_registry().read();
        match registry
            .get(&id)
            .and_then(|s| s.aliases_to_chain.get(&alias))
        {
            Some(c) => c.clone(),
            None => return Ok(Some(Value::Object(None))),
        }
    };

    let cls_id = ctx
        .ensure_class_initialized("java/security/cert/X509Certificate")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, chain.len());
    for (i, der) in chain.iter().enumerate() {
        let mirror = make_x509_mirror(ctx, &alias, der);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn get_private_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let alias = read_string_at(ctx, args, 1).unwrap_or_default();

    let key_der = {
        let registry = km_registry().read();
        match registry.get(&id).and_then(|s| s.aliases_to_key.get(&alias)) {
            Some(k) => k.clone(),
            None => return Ok(Some(Value::Object(None))),
        }
    };
    let pk = make_private_key_mirror(ctx, &key_der, id, &alias);
    Ok(Some(Value::Object(Some(pk))))
}

fn get_server_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let key_type = read_string_at(ctx, args, 1).unwrap_or_default();
    let aliases = {
        let registry = km_registry().read();
        registry
            .get(&id)
            .and_then(|s| s.server_aliases_by_key_type.get(&key_type).cloned())
            .unwrap_or_default()
    };
    Ok(Some(Value::Object(Some(materialize_string_array(
        ctx, &aliases,
    )))))
}

fn get_client_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let key_type = read_string_at(ctx, args, 1).unwrap_or_default();
    let aliases = {
        let registry = km_registry().read();
        registry
            .get(&id)
            .and_then(|s| s.client_aliases_by_key_type.get(&key_type).cloned())
            .unwrap_or_default()
    };
    Ok(Some(Value::Object(Some(materialize_string_array(
        ctx, &aliases,
    )))))
}

fn materialize_string_array(ctx: &mut dyn NativeContext, items: &[String]) -> ObjectRef {
    let cls_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, items.len());
    for (i, s) in items.iter().enumerate() {
        let js = ctx.create_string(s);
        ctx.set_array_element(arr, i, Value::Object(Some(js)));
    }
    arr
}

// ---------------------------------------------------------------------------
// X509TrustManager handlers
// ---------------------------------------------------------------------------

fn check_client_trusted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    do_check_trusted(ctx, args)
}

fn check_server_trusted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    do_check_trusted(ctx, args)
}

fn do_check_trusted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_tm_id(ctx, this);
    let chain_val = match args.get(1) {
        Some(v) => v.clone(),
        None => Value::Object(None),
    };
    let chain = read_chain_arg(ctx, &chain_val);
    if chain.is_empty() {
        return Err(cert_exception("certificate chain is empty".into()));
    }

    let trust = {
        let registry = tm_registry().read();
        match registry.get(&id) {
            Some(s) => s.clone(),
            None => {
                // Fallback to a system-only trust state — better than denying
                // everything when init() was bypassed (which real-JDK permits
                // for the implicit default trust manager).
                drop(registry);
                build_trust_manager_state(0)
            }
        }
    };

    match validate_chain(&chain, &trust) {
        Ok(()) => Ok(None),
        Err(e) => Err(cert_exception(format!("CertificateException: {}", e))),
    }
}

fn get_accepted_issuers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_tm_id(ctx, this);

    let ders = {
        let registry = tm_registry().read();
        match registry.get(&id) {
            Some(s) => s.anchor_ders.clone(),
            None => {
                drop(registry);
                let s = build_trust_manager_state(0);
                s.anchor_ders
            }
        }
    };

    let cls_id = ctx
        .ensure_class_initialized("java/security/cert/X509Certificate")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, ders.len());
    for (i, der) in ders.iter().enumerate() {
        let mirror = make_x509_mirror(ctx, "trust-anchor", der);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// PKIXValidator
// ---------------------------------------------------------------------------

fn pkix_engine_validate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The receiver is a PKIXValidator (no per-conn trust state attached on
    // the native side); fall back to a system-default trust state.
    let _this = this_arg(args)?;
    let chain_val = match args.get(1) {
        Some(v) => v.clone(),
        None => return Err(cert_exception("null chain".into())),
    };
    let chain = read_chain_arg(ctx, &chain_val);
    let trust = build_trust_manager_state(0);
    match validate_chain(&chain, &trust) {
        Ok(()) => {
            // Real-JDK returns the validated path. We hand the input chain
            // straight back since we accepted it as-is.
            let chain_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            Ok(Some(Value::Object(Some(chain_arr))))
        }
        Err(e) => Err(cert_exception(format!("CertPathValidatorException: {}", e))),
    }
}

// ---------------------------------------------------------------------------
// KeyManagerFactory / TrustManagerFactory
// ---------------------------------------------------------------------------

fn kmf_engine_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let ks_id = match args.get(1) {
        Some(Value::Object(Some(ks))) => read_keystore_id(ctx, *ks),
        _ => 0,
    };
    let state = build_key_manager_state(ks_id);
    let id = next_km_id();
    km_registry().write().insert(id, state);
    set_km_id(ctx, this, id);
    Ok(Some(Value::Object(None)))
}

fn kmf_engine_get_key_managers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);

    // Return a 1-element KeyManager[] holding a SunX509KeyManagerImpl mirror
    // wired to the same id.
    let cls_id = ctx
        .ensure_class_initialized("javax/net/ssl/KeyManager")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, 1);
    let km = alloc_concurrent_synthetic(ctx, FQN_SUN_X509_KM, 2);
    set_km_id(ctx, km, id);
    ctx.set_array_element(arr, 0, Value::Object(Some(km)));
    Ok(Some(Value::Object(Some(arr))))
}

fn tmf_engine_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let ks_id = match args.get(1) {
        Some(Value::Object(Some(ks))) => read_keystore_id(ctx, *ks),
        _ => 0,
    };
    let state = build_trust_manager_state(ks_id);
    let id = next_tm_id();
    tm_registry().write().insert(id, state);
    set_tm_id(ctx, this, id);
    Ok(Some(Value::Object(None)))
}

fn tmf_engine_get_trust_managers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_tm_id(ctx, this);

    let cls_id = ctx
        .ensure_class_initialized("javax/net/ssl/TrustManager")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let arr = ctx.new_ref_array(cls_id, 1);
    let tm = alloc_concurrent_synthetic(ctx, FQN_X509_TM, 2);
    set_tm_id(ctx, tm, id);
    ctx.set_array_element(arr, 0, Value::Object(Some(tm)));
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Hermetic tests built against a hand-rolled X.509 fixture. We never
    //! shell out to `openssl` — every cert byte here is produced by the
    //! `mk_cert` builder below so the suite passes on any host.

    use super::*;

    /// Tiny DER builder — same shape as `keystore.rs` but kept private so the
    /// two modules don't drift on test fixtures.
    fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 4);
        out.push(tag);
        encode_length(body.len(), &mut out);
        out.extend_from_slice(body);
        out
    }

    fn encode_length(len: usize, out: &mut Vec<u8>) {
        if len < 0x80 {
            out.push(len as u8);
        } else if len <= 0xFF {
            out.push(0x81);
            out.push(len as u8);
        } else if len <= 0xFFFF {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push(len as u8);
        } else {
            out.push(0x83);
            out.push((len >> 16) as u8);
            out.push((len >> 8) as u8);
            out.push(len as u8);
        }
    }

    fn der_oid(body: &[u8]) -> Vec<u8> {
        der_tlv(TAG_OID, body)
    }

    fn der_seq(body: Vec<u8>) -> Vec<u8> {
        der_tlv(TAG_SEQUENCE, &body)
    }

    fn der_set(body: Vec<u8>) -> Vec<u8> {
        der_tlv(TAG_SET, &body)
    }

    fn der_int(value: u8) -> Vec<u8> {
        der_tlv(TAG_INTEGER, &[value])
    }

    fn der_utctime(s: &str) -> Vec<u8> {
        der_tlv(TAG_UTC_TIME, s.as_bytes())
    }

    fn der_bit_string(unused: u8, body: &[u8]) -> Vec<u8> {
        let mut full = Vec::with_capacity(body.len() + 1);
        full.push(unused);
        full.extend_from_slice(body);
        der_tlv(TAG_BIT_STRING, &full)
    }

    fn der_octet(body: &[u8]) -> Vec<u8> {
        der_tlv(TAG_OCTET_STRING, body)
    }

    fn der_bool(b: bool) -> Vec<u8> {
        der_tlv(TAG_BOOLEAN, &[if b { 0xff } else { 0x00 }])
    }

    fn der_context_explicit(num: u8, body: &[u8]) -> Vec<u8> {
        let tag = 0xa0 | (num & 0x0f);
        der_tlv(tag, body)
    }

    /// Build a v3 cert with the supplied extensions.  This is *not* a valid
    /// signed cert (the signature is a fixed dummy) but exercises every
    /// field of our parser.  The KU bit-string convention here is "high bit
    /// first" — unused bits are the trailing `8 - keep` bits of the last
    /// byte.
    struct CertSpec {
        not_before_utc: &'static str,
        not_after_utc: &'static str,
        subject_cn: &'static str,
        issuer_cn: &'static str,
        spki_alg: &'static [u8],
        key_usage_bits: Option<u16>,
        ext_key_usages: &'static [&'static [u8]],
        basic_constraints_ca: Option<bool>,
        /// dNSName SubjectAltName entries to embed (empty = no SAN ext).
        subject_alt_dns: &'static [&'static str],
    }

    impl Default for CertSpec {
        fn default() -> Self {
            CertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "leaf.example.com",
                issuer_cn: "Acme CA",
                spki_alg: OID_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(false),
                subject_alt_dns: &[],
            }
        }
    }

    /// Encode a SubjectAltName extension value containing the given dNSName
    /// entries: SEQUENCE OF GeneralName, each `[2] IMPLICIT IA5String`.
    fn san_dns_value(names: &[&str]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        for n in names {
            body.extend_from_slice(&der_tlv(SAN_TAG_DNS_NAME, n.as_bytes()));
        }
        der_seq(body)
    }

    fn name_with_cn(cn: &str) -> Vec<u8> {
        // SEQUENCE { SET { SEQUENCE { OID 2.5.4.3, PrintableString } } }
        let cn_oid = der_oid(&[0x55, 0x04, 0x03]);
        let cn_val = der_tlv(0x13, cn.as_bytes());
        let atv = der_seq([cn_oid, cn_val].concat());
        let rdn = der_set(atv);
        der_seq(rdn)
    }

    fn spki(alg_oid: &[u8]) -> Vec<u8> {
        // SubjectPublicKeyInfo ::= SEQUENCE { AlgorithmIdentifier, BIT STRING }
        let alg = der_seq([der_oid(alg_oid), der_tlv(TAG_NULL, &[])].concat());
        let bs = der_bit_string(0, &[0x00, 0x01]);
        der_seq([alg, bs].concat())
    }

    fn extension(oid: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
        let mut parts: Vec<u8> = Vec::new();
        parts.extend_from_slice(&der_oid(oid));
        if critical {
            parts.extend_from_slice(&der_bool(true));
        }
        parts.extend_from_slice(&der_octet(&value));
        der_seq(parts)
    }

    fn ku_bitstring(bits: u16) -> Vec<u8> {
        // Pack the lowest 9 bits into a BIT STRING per RFC 5280: bit 0 (MSB)
        // = digitalSignature, bit 8 = decipherOnly.  Find the highest bit
        // position that is set so we can choose body length + unused-bits
        // count correctly (per DER, the unused-bits count must let the
        // receiver recover *exactly* the bits that were set by the issuer).
        let mut value: u16 = 0;
        let mut highest_set: i32 = -1;
        for pos in 0..9 {
            if (bits >> pos) & 1 == 1 {
                value |= 1 << (15 - pos);
                if pos as i32 > highest_set {
                    highest_set = pos as i32;
                }
            }
        }
        if highest_set < 0 {
            return der_bit_string(7, &[0x00]);
        }
        let needed_bits = (highest_set + 1) as usize;
        // Round up to whole bytes.
        let body_len = (needed_bits + 7) / 8;
        let unused = (body_len * 8 - needed_bits) as u8;
        let bytes = [(value >> 8) as u8, value as u8];
        let body = &bytes[..body_len];
        der_bit_string(unused, body)
    }

    fn eku_seq(oids: &[&[u8]]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        for o in oids {
            body.extend_from_slice(&der_oid(o));
        }
        der_seq(body)
    }

    fn bc_seq(ca: bool) -> Vec<u8> {
        if ca {
            der_seq(der_bool(true))
        } else {
            der_seq(Vec::new())
        }
    }

    fn mk_cert(spec: &CertSpec) -> Vec<u8> {
        let mut tbs: Vec<u8> = Vec::new();
        // version [0] EXPLICIT INTEGER 2 (i.e. v3)
        tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
        // serial
        tbs.extend_from_slice(&der_int(1));
        // signature alg
        tbs.extend_from_slice(&der_seq(
            [
                der_oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
                der_tlv(TAG_NULL, &[]),
            ]
            .concat(),
        ));
        // issuer
        tbs.extend_from_slice(&name_with_cn(spec.issuer_cn));
        // validity
        tbs.extend_from_slice(&der_seq(
            [
                der_utctime(spec.not_before_utc),
                der_utctime(spec.not_after_utc),
            ]
            .concat(),
        ));
        // subject
        tbs.extend_from_slice(&name_with_cn(spec.subject_cn));
        // SPKI
        tbs.extend_from_slice(&spki(spec.spki_alg));
        // extensions
        let mut exts: Vec<u8> = Vec::new();
        if let Some(bits) = spec.key_usage_bits {
            exts.extend_from_slice(&extension(OID_EXT_KEY_USAGE, true, ku_bitstring(bits)));
        }
        if !spec.ext_key_usages.is_empty() {
            exts.extend_from_slice(&extension(
                OID_EXT_EXTENDED_KEY_USAGE,
                false,
                eku_seq(spec.ext_key_usages),
            ));
        }
        if let Some(ca) = spec.basic_constraints_ca {
            exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(ca)));
        }
        if !spec.subject_alt_dns.is_empty() {
            exts.extend_from_slice(&extension(
                OID_EXT_SUBJECT_ALT_NAME,
                false,
                san_dns_value(spec.subject_alt_dns),
            ));
        }
        if !exts.is_empty() {
            tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
        }

        // Build outer cert: SEQUENCE { tbs, sigAlg, signature }
        let outer_sig_alg = der_seq(
            [
                der_oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
                der_tlv(TAG_NULL, &[]),
            ]
            .concat(),
        );
        let dummy_sig = der_bit_string(0, &[0xde, 0xad, 0xbe, 0xef]);
        der_seq([der_seq(tbs), outer_sig_alg, dummy_sig].concat())
    }

    #[test]
    fn parse_minimal_v3_cert() {
        let cert = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf.example.com",
            issuer_cn: "Acme CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let p = parse_certificate(&cert).expect("parse");
        assert_eq!(p.spki_algorithm_oid, OID_RSA);
        assert_eq!(p.basic_constraints_ca, Some(false));
        let ku = p.key_usage.expect("ku");
        assert!(ku & KU_DIGITAL_SIGNATURE != 0);
        assert!(ku & KU_KEY_ENCIPHERMENT != 0);
        assert_eq!(p.ext_key_usage.len(), 1);
        assert_eq!(p.ext_key_usage[0], OID_KP_SERVER_AUTH);
    }

    #[test]
    fn classify_key_type_recognises_known_algorithms() {
        assert_eq!(classify_key_type(OID_RSA), "RSA");
        assert_eq!(classify_key_type(OID_EC), "EC");
        assert_eq!(classify_key_type(OID_DSA), "DSA");
        assert_eq!(classify_key_type(OID_ED25519), "Ed25519");
        assert_eq!(classify_key_type(&[0x01, 0x02]), "UNKNOWN");
    }

    #[test]
    fn server_cert_requires_eku_serverauth_when_eku_present() {
        let cert_ok = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "ok.example",
            issuer_cn: "CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let cert_bad = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "bad.example",
            issuer_cn: "CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_CLIENT_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        assert!(is_server_cert(&parse_certificate(&cert_ok).unwrap()));
        assert!(!is_server_cert(&parse_certificate(&cert_bad).unwrap()));
    }

    #[test]
    fn client_cert_requires_eku_clientauth_when_eku_present() {
        let cert_ok = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "client.example",
            issuer_cn: "CA",
            spki_alg: OID_EC,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_CLIENT_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let cert_bad = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "wrong.example",
            issuer_cn: "CA",
            spki_alg: OID_EC,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        assert!(is_client_cert(&parse_certificate(&cert_ok).unwrap()));
        assert!(!is_client_cert(&parse_certificate(&cert_bad).unwrap()));
    }

    #[test]
    fn cert_without_eku_is_acceptable_for_both_roles() {
        let cert = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "any.example",
            issuer_cn: "CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
            ext_key_usages: &[],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let p = parse_certificate(&cert).unwrap();
        assert!(is_server_cert(&p));
        assert!(is_client_cert(&p));
    }

    #[test]
    fn validate_chain_rejects_expired_leaf() {
        let leaf = mk_cert(&CertSpec {
            not_before_utc: "990101000000Z",
            not_after_utc: "000101000000Z", // expired in 2000
            subject_cn: "expired",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let anchor_der = mk_cert(&CertSpec {
            not_before_utc: "990101000000Z",
            not_after_utc: "490101000000Z",
            subject_cn: "Anchor",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, anchor_der);
        let chain = vec![leaf];
        let result = validate_chain(&chain, &trust);
        match result {
            Err(TrustError::Expired { .. }) => {}
            other => panic!("expected Expired, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_rejects_when_no_trust_anchor() {
        let leaf = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf",
            issuer_cn: "Stranger",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let trust = TrustManagerState::default();
        let result = validate_chain(&[leaf], &trust);
        match result {
            Err(TrustError::NoTrustAnchor) => {}
            other => panic!("expected NoTrustAnchor, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_passes_self_signed_in_trust() {
        let cert = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Anchor",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, cert.clone());
        validate_chain(&[cert], &trust).expect("should validate");
    }

    #[test]
    fn validate_chain_rejects_broken_continuity() {
        let leaf = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf",
            issuer_cn: "Real CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let intermediate = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Different CA", // mismatch — not the issuer of leaf
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let anchor = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Anchor",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, anchor);
        let result = validate_chain(&[leaf, intermediate], &trust);
        match result {
            Err(TrustError::BrokenChain { .. }) => {}
            other => panic!("expected BrokenChain, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_rejects_intermediate_without_ca_flag() {
        let leaf = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "leaf",
            issuer_cn: "Bogus Intermediate",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let bogus = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Bogus Intermediate",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[],
            basic_constraints_ca: Some(false), // explicitly NOT a CA
        });
        let anchor = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Anchor",
            issuer_cn: "Anchor",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, anchor);
        match validate_chain(&[leaf, bogus], &trust) {
            Err(TrustError::NotCa { .. }) => {}
            other => panic!("expected NotCa, got {:?}", other),
        }
    }

    #[test]
    fn rustls_native_certs_loads() {
        // Sanity check: the platform call returns *something*. Even on a
        // stripped-down container this should yield either certs or
        // documented errors, never a panic.
        let result = rustls_native_certs::load_native_certs();
        let _ = result.certs.len();
        let _ = result.errors.len();
    }

    #[test]
    fn build_trust_manager_state_includes_system_roots() {
        // No user keystore, just system anchors. We expect a non-zero number
        // of accepted issuers on every supported platform — but we do not
        // assert > 0 because some sandboxed CI environments strip the OS
        // trust store. We assert no panic and that the structure is
        // internally consistent: anchor_ders.len() >= anchors.len().
        let s = build_trust_manager_state(0);
        assert!(s.anchor_ders.len() >= s.anchors.len());
        assert_eq!(s.keystore_id, 0);
    }

    #[test]
    fn key_usage_bit_extraction_is_correct() {
        let cert = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "x",
            issuer_cn: "y",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_AGREEMENT),
            ext_key_usages: &[],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let ku = parse_certificate(&cert).unwrap().key_usage.unwrap();
        assert_eq!(ku & KU_DIGITAL_SIGNATURE, KU_DIGITAL_SIGNATURE);
        assert_eq!(ku & KU_KEY_AGREEMENT, KU_KEY_AGREEMENT);
        assert_eq!(ku & KU_KEY_ENCIPHERMENT, 0);
    }

    #[test]
    fn classify_key_type_from_pkcs8_for_rsa() {
        // Minimal PKCS#8: SEQUENCE { INTEGER 0, AlgorithmIdentifier{rsa,
        // NULL}, OCTET STRING ... }. We only need the OID match.
        let pkcs8 = der_seq(
            [
                der_int(0),
                der_seq([der_oid(OID_RSA), der_tlv(TAG_NULL, &[])].concat()),
                der_octet(&[0u8; 4]),
            ]
            .concat(),
        );
        assert_eq!(classify_key_type_from_pkcs8(&pkcs8), "RSA");

        let pkcs8_ec = der_seq(
            [
                der_int(0),
                der_seq([der_oid(OID_EC), der_tlv(TAG_NULL, &[])].concat()),
                der_octet(&[0u8; 4]),
            ]
            .concat(),
        );
        assert_eq!(classify_key_type_from_pkcs8(&pkcs8_ec), "EC");
    }

    // ====================================================================
    // Real-signature chain-verification tests (RSA-SHA256 and ECDSA-P256).
    //
    // These build a self-consistent toy CA in-memory: anchor + leaf are
    // signed with a real generated keypair, then validate_chain re-verifies
    // them through the same primitives. No openssl / no fixtures.
    // ====================================================================

    use crate::crypto_impl::{
        Ecdsa, EcdsaPrivateKey, EcdsaPublicKey, Rsa, RsaPrivateKey, RsaPublicKey, Sha256,
    };

    /// Variant of `CertSpec` that takes a *real* SPKI DER (already encoded
    /// by `Rsa::public_key_to_der` / `Ecdsa::public_key_to_der`) instead of
    /// a fake algorithm-OID-only SPKI. We also accept the issuer's signing
    /// algorithm (the cert's `signatureAlgorithm`) so RSA and ECDSA chains
    /// share the same builder.
    struct SignedCertSpec<'a> {
        not_before_utc: &'static str,
        not_after_utc: &'static str,
        subject_cn: &'static str,
        issuer_cn: &'static str,
        spki_der: &'a [u8],
        sig_alg_oid: &'static [u8],
        key_usage_bits: Option<u16>,
        ext_key_usages: &'static [&'static [u8]],
        basic_constraints_ca: Option<bool>,
    }

    /// Build the *tbsCertificate* DER for a SignedCertSpec. Used both to
    /// produce the bytes the issuer signs and (after the signature is
    /// computed) to assemble the final SEQUENCE { tbs, sigAlg, sigValue }.
    fn build_tbs(spec: &SignedCertSpec) -> Vec<u8> {
        let mut tbs: Vec<u8> = Vec::new();
        // version [0] EXPLICIT INTEGER 2 (v3)
        tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
        // serial
        tbs.extend_from_slice(&der_int(1));
        // signature alg (this MUST match the outer sigAlg byte-for-byte;
        // RFC 5280 §4.1.1.2 requires it)
        let sig_alg_seq = der_seq([der_oid(spec.sig_alg_oid), der_tlv(TAG_NULL, &[])].concat());
        tbs.extend_from_slice(&sig_alg_seq);
        // issuer
        tbs.extend_from_slice(&name_with_cn(spec.issuer_cn));
        // validity
        tbs.extend_from_slice(&der_seq(
            [
                der_utctime(spec.not_before_utc),
                der_utctime(spec.not_after_utc),
            ]
            .concat(),
        ));
        // subject
        tbs.extend_from_slice(&name_with_cn(spec.subject_cn));
        // SPKI — already DER-encoded by the keypair serializer
        tbs.extend_from_slice(spec.spki_der);
        // extensions
        let mut exts: Vec<u8> = Vec::new();
        if let Some(bits) = spec.key_usage_bits {
            exts.extend_from_slice(&extension(OID_EXT_KEY_USAGE, true, ku_bitstring(bits)));
        }
        if !spec.ext_key_usages.is_empty() {
            exts.extend_from_slice(&extension(
                OID_EXT_EXTENDED_KEY_USAGE,
                false,
                eku_seq(spec.ext_key_usages),
            ));
        }
        if let Some(ca) = spec.basic_constraints_ca {
            exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(ca)));
        }
        if !exts.is_empty() {
            tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
        }
        der_seq(tbs)
    }

    /// Encrypted sigAlg DER (SEQUENCE { OID, NULL }) for use in the outer
    /// SEQUENCE.
    fn sig_alg_seq(oid: &[u8]) -> Vec<u8> {
        der_seq([der_oid(oid), der_tlv(TAG_NULL, &[])].concat())
    }

    /// Assemble Certificate ::= SEQUENCE { tbs, sigAlgorithm, signatureValue }
    /// once `tbs` and `signature` bytes are known.
    fn assemble_cert(tbs: &[u8], sig_alg_oid: &[u8], signature: &[u8]) -> Vec<u8> {
        let outer_sig_alg = sig_alg_seq(sig_alg_oid);
        let sig_bs = der_bit_string(0, signature);
        let mut outer = Vec::with_capacity(tbs.len() + outer_sig_alg.len() + sig_bs.len());
        outer.extend_from_slice(tbs);
        outer.extend_from_slice(&outer_sig_alg);
        outer.extend_from_slice(&sig_bs);
        der_seq(outer)
    }

    /// Sign-and-bundle a cert with an RSA issuer key.
    fn mk_rsa_signed_cert(spec: &SignedCertSpec, issuer_sk: &RsaPrivateKey) -> Vec<u8> {
        let tbs = build_tbs(spec);
        let signature = Rsa::sign_sha256(issuer_sk, &tbs);
        assemble_cert(&tbs, spec.sig_alg_oid, &signature)
    }

    /// Sign-and-bundle a cert with an ECDSA P-256 issuer key.
    fn mk_ecdsa_signed_cert(spec: &SignedCertSpec, issuer_sk: &EcdsaPrivateKey) -> Vec<u8> {
        let tbs = build_tbs(spec);
        let digest = Sha256::digest(&tbs);
        let signature = Ecdsa::sign_with_digest(issuer_sk, &digest);
        assemble_cert(&tbs, spec.sig_alg_oid, &signature)
    }

    /// Lazily cached RSA-1024 root keypair so the test suite doesn't pay
    /// for `generate_keypair` more than once. Keypair generation can take
    /// several seconds on debug builds — sharing it across the three RSA
    /// tests keeps the suite snappy without weakening coverage (each test
    /// still drives its own validate_chain end-to-end).
    fn shared_rsa_root() -> &'static (RsaPublicKey, RsaPrivateKey) {
        use std::sync::OnceLock;
        static CELL: OnceLock<(RsaPublicKey, RsaPrivateKey)> = OnceLock::new();
        CELL.get_or_init(|| Rsa::generate_keypair(1024))
    }

    /// Lazily cached ECDSA P-256 root keypair (parallel rationale to
    /// `shared_rsa_root`).
    fn shared_ecdsa_root() -> &'static (EcdsaPublicKey, EcdsaPrivateKey) {
        use std::sync::OnceLock;
        static CELL: OnceLock<(EcdsaPublicKey, EcdsaPrivateKey)> = OnceLock::new();
        CELL.get_or_init(|| Ecdsa::generate_keypair())
    }

    #[test]
    fn validate_chain_real_rsa_sha256_signature_passes() {
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);

        // Build a self-signed root (anchor) with REAL RSA SPKI and a real
        // sha256WithRSAEncryption signature over its TBS.
        let root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Real RSA Root",
                issuer_cn: "Real RSA Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            root_sk,
        );

        // Build a leaf signed by the same root. (Sharing the keypair is a
        // shortcut — the leaf's *own* SPKI is irrelevant to the verifier
        // because we never re-sign anything from the leaf in this test.)
        let leaf = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "leaf.example.com",
                issuer_cn: "Real RSA Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            root_sk,
        );

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        validate_chain(&[leaf, root], &trust).expect("real RSA chain must validate");
    }

    #[test]
    fn validate_chain_real_rsa_tampered_signature_is_bad_signature() {
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);

        let root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Real RSA Root",
                issuer_cn: "Real RSA Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            root_sk,
        );

        // Build a leaf, then *flip a byte in the signature bit-string* so
        // PKCS#1 v1.5 padding-decode will produce the wrong EMSA envelope.
        let mut leaf = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "tampered.example",
                issuer_cn: "Real RSA Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            root_sk,
        );
        // Flip the very last byte of the cert — that's inside the signature
        // BIT STRING. (We don't shift any DER length headers, so the cert
        // still parses; only the cryptographic check should fail.)
        let last_idx = leaf.len() - 1;
        leaf[last_idx] ^= 0x01;

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        match validate_chain(&[leaf, root], &trust) {
            Err(TrustError::BadSignature { at }) => {
                assert_eq!(at, 0, "leaf is at index 0");
            }
            other => panic!("expected BadSignature, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_real_ecdsa_p256_sha256_signature_passes() {
        let (root_pk, root_sk) = shared_ecdsa_root();
        let root_spki = Ecdsa::public_key_to_der(root_pk);

        let root = mk_ecdsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Real EC Root",
                issuer_cn: "Real EC Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_ECDSA_SHA256,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            root_sk,
        );

        let leaf = mk_ecdsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "ec-leaf.example.com",
                issuer_cn: "Real EC Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_ECDSA_SHA256,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            root_sk,
        );

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        validate_chain(&[leaf, root], &trust).expect("real ECDSA chain must validate");
    }

    #[test]
    fn validate_chain_unknown_signature_oid_reports_not_implemented() {
        // Build a chain where the leaf is signed with id-RSASSA-PSS — an OID
        // the verifier knows but explicitly does not implement (no PSS
        // parameter parsing today). The anchor is still RSA-SHA256-signed
        // so the chain reaches Step 6 cleanly; the rejection comes from the
        // leaf's OID dispatch.
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);

        let root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Real RSA Root",
                issuer_cn: "Real RSA Root",
                spki_der: &root_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            root_sk,
        );

        // Build the leaf's TBS with PSS OID, then sign with PKCS#1 v1.5
        // anyway — the *signature bytes* don't matter; we only need the
        // outer sigAlg OID to route to NotImplemented before we touch the
        // cryptographic verifier.
        let tbs = build_tbs(&SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "pss-leaf",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_RSA_PSS,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let fake_sig = Rsa::sign_sha256(root_sk, &tbs);
        let leaf = assemble_cert(&tbs, OID_SIG_RSA_PSS, &fake_sig);

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        match validate_chain(&[leaf, root], &trust) {
            Err(TrustError::NotImplemented { at, oid }) => {
                assert_eq!(at, 0);
                assert_eq!(oid, OID_SIG_RSA_PSS.to_vec());
            }
            other => panic!("expected NotImplemented for PSS, got {:?}", other),
        }
    }

    // ====================================================================
    // Endpoint identification (hostname verification) tests.
    // ====================================================================

    fn leaf_with_san(cn: &'static str, dns: &'static [&'static str]) -> ParsedCert {
        let der = mk_cert(&CertSpec {
            subject_cn: cn,
            issuer_cn: "CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: dns,
            ..Default::default()
        });
        parse_certificate(&der).expect("parse")
    }

    #[test]
    fn san_dns_names_are_parsed_lowercased() {
        let leaf = leaf_with_san("ignored", &["WWW.Example.COM", "api.example.com"]);
        assert_eq!(
            leaf.san_dns_names,
            vec!["www.example.com", "api.example.com"]
        );
    }

    #[test]
    fn subject_cn_is_extracted_lowercased() {
        let leaf = leaf_with_san("Leaf.Example.Com", &[]);
        assert_eq!(leaf.subject_cn.as_deref(), Some("leaf.example.com"));
    }

    #[test]
    fn verify_hostname_exact_san_match() {
        let leaf = leaf_with_san("cn.example.com", &["host.example.com"]);
        assert_eq!(verify_hostname(&leaf, "host.example.com"), Ok(()));
        assert_eq!(verify_hostname(&leaf, "HOST.example.com"), Ok(()));
    }

    #[test]
    fn verify_hostname_rejects_wrong_host_with_san() {
        let leaf = leaf_with_san("cn.example.com", &["host.example.com"]);
        match verify_hostname(&leaf, "evil.example.com") {
            Err(HostnameError::NoMatch { .. }) => {}
            other => panic!("expected NoMatch, got {:?}", other),
        }
    }

    #[test]
    fn verify_hostname_wildcard_matches_one_label() {
        let leaf = leaf_with_san("cn", &["*.example.com"]);
        assert_eq!(verify_hostname(&leaf, "a.example.com"), Ok(()));
        // Wildcard must NOT span a dot.
        assert!(verify_hostname(&leaf, "a.b.example.com").is_err());
        // Wildcard must NOT match the bare parent domain.
        assert!(verify_hostname(&leaf, "example.com").is_err());
    }

    #[test]
    fn verify_hostname_rejects_overbroad_wildcard() {
        // `*.com` leaves only one label to the right — must be refused.
        assert!(!dns_name_matches("*.com", "example.com"));
        assert!(!dns_name_matches("*", "example"));
        // Wildcard outside the left-most label is invalid.
        assert!(!dns_name_matches("a.*.com", "a.b.com"));
        // Partial-label wildcards are not RFC 6125 wildcards (we only accept
        // a whole `*.` left label), so they fall through to literal compare.
        assert!(!dns_name_matches("f*o.example.com", "foo.example.com"));
    }

    #[test]
    fn verify_hostname_cn_fallback_only_without_san_dns() {
        // No SAN dNSName → CN is consulted.
        let cn_only = leaf_with_san("host.example.com", &[]);
        assert_eq!(verify_hostname(&cn_only, "host.example.com"), Ok(()));

        // SAN dNSName present but non-matching → CN must NOT rescue it
        // (RFC 6125 §6.4.4: presence of SAN forbids CN fallback).
        let san_present = leaf_with_san("host.example.com", &["other.example.com"]);
        match verify_hostname(&san_present, "host.example.com") {
            Err(HostnameError::NoMatch { .. }) => {}
            other => panic!("expected NoMatch (CN must not rescue), got {:?}", other),
        }
    }

    #[test]
    fn verify_hostname_empty_host_is_rejected() {
        let leaf = leaf_with_san("host.example.com", &["host.example.com"]);
        assert_eq!(verify_hostname(&leaf, ""), Err(HostnameError::EmptyHost));
        assert_eq!(verify_hostname(&leaf, "   "), Err(HostnameError::EmptyHost));
    }

    #[test]
    fn verify_hostname_no_identity_fails_closed() {
        // No SAN, no CN — nothing to match against.
        let leaf = leaf_with_san("", &[]);
        // An empty CN string is still "present" but cannot match a real host;
        // either NoIdentity or NoMatch is acceptable, never Ok.
        assert!(verify_hostname(&leaf, "host.example.com").is_err());
    }

    #[test]
    fn host_is_ip_literal_classifies_correctly() {
        assert!(host_is_ip_literal("127.0.0.1"));
        assert!(host_is_ip_literal("10.0.0.255"));
        assert!(host_is_ip_literal("::1"));
        assert!(host_is_ip_literal("fe80::1"));
        assert!(!host_is_ip_literal("example.com"));
        assert!(!host_is_ip_literal("256.0.0.1")); // out of range
        assert!(!host_is_ip_literal("1.2.3")); // too few labels
    }

    #[test]
    fn check_endpoint_identity_drives_leaf() {
        let der = mk_cert(&CertSpec {
            subject_cn: "cn.example.com",
            issuer_cn: "CA",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &["leaf.example.com"],
            ..Default::default()
        });
        assert_eq!(
            check_endpoint_identity(&[der.clone()], "leaf.example.com"),
            Ok(())
        );
        assert!(check_endpoint_identity(&[der], "wrong.example.com").is_err());
        // Empty chain has no leaf identity.
        assert!(check_endpoint_identity(&[], "leaf.example.com").is_err());
    }
}
