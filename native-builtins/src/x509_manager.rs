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
//!   5. The chain terminates at the actual configured trust anchor, or at a
//!      certificate signed by that anchor. Same-subject certificates supplied
//!      by the peer are verified against the stored anchor's SPKI.
//!   6. Name constraints (RFC 5280 §4.2.1.10) — every CA's `NameConstraints`
//!      extension (permitted / excluded subtrees) is enforced against the
//!      subject DN and SubjectAltName of every certificate beneath it in the
//!      path (and against a name-constrained root even when that root is not
//!      shipped in the chain). The `GeneralName` types evaluated are dNSName,
//!      rfc822Name, uniformResourceIdentifier (host), iPAddress (CIDR), and
//!      directoryName (RDN prefix). Self-issued non-leaf certs are exempt
//!      (§6.1.3(b)). Residual limits: subtree types we do not model (otherName,
//!      x400Address, ediPartyName, registeredID) are not enforced, and
//!      directoryName matching is byte-exact per RDN (no string-value
//!      normalisation). See `check_name_constraints`.
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
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
use parking_lot::RwLock;

use crate::try_alloc_concurrent_synthetic;
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
    /// alias -> the LIVE Java `PrivateKey` object this manager must hand back,
    /// for a key that has no PKCS#8 encoding of its own.
    ///
    /// `aliases_to_key` stores key BYTES, which is enough for every key whose
    /// `getEncoded()` answers. An *opaque* key — a PKCS#11/HSM key, or netty's
    /// `OpenSslPrivateKey` and `OpenSslPrivateKeyMethod` delegating keys, whose
    /// whole point is that the private material never leaves its provider —
    /// answers `null` there, and reconstructing one from bytes is not merely
    /// lossy but impossible. The caller needs THAT object back:
    /// `OpenSslKeyMaterialProvider.chooseKeyMaterial` branches on
    /// `key instanceof OpenSslPrivateKey` to decide whether to hand the key to
    /// OpenSSL by reference or to PEM-encode it.
    ///
    /// Holds live `ObjectRef`s, so `km_registry` is scanned and remapped by
    /// [`gc_scan_key_manager_roots`] / [`gc_update_key_manager_refs`].
    pub aliases_to_live_key: HashMap<String, ObjectRef>,
}

/// GC root scan for the live `PrivateKey` objects a `KeyManagerState` holds —
/// see [`KeyManagerState::aliases_to_live_key`].
pub fn gc_scan_key_manager_roots(roots: &mut Vec<ObjectRef>) {
    for state in km_registry().read().values() {
        for k in state.aliases_to_live_key.values() {
            if !k.as_ptr().is_null() {
                roots.push(*k);
            }
        }
    }
}

/// Post-move remap companion to [`gc_scan_key_manager_roots`].
pub fn gc_update_key_manager_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    for state in km_registry().write().values_mut() {
        for k in state.aliases_to_live_key.values_mut() {
            let old = k.as_ptr() as usize;
            if let Some(&new) = map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: `new` is a live, 8-byte-aligned heap address produced
                // by the moving collector for the object previously at `old`.
                *k = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
}

/// Trust-manager state. A null/default `TrustManagerFactory.init` state is
/// populated from the system trust store; an explicit caller `KeyStore` is
/// restrictive and contains only that store's anchors. Anchors are grouped by
/// subject-DN DER for fast chain-end matching, but validation still selects a
/// concrete stored anchor by exact DER/SPKI match or by verifying the chain end
/// against an anchor SPKI. `anchor_ders` is the original DER bytes so
/// `getAcceptedIssuers()` can return them as `X509Certificate[]`.
#[derive(Clone, Debug, Default)]
pub struct TrustManagerState {
    pub keystore_id: i32,
    /// subject_dn_der -> trust anchors with that subject.
    pub anchors: HashMap<Vec<u8>, Vec<AnchorInfo>>,
    /// The full DER of every trust anchor, in registration order.
    pub anchor_ders: Vec<Vec<u8>>,
    /// Real OCSP/CRL revocation-checking configuration, extracted from a
    /// `java.security.cert.PKIXRevocationChecker` attached via
    /// `PKIXBuilderParameters.addCertPathChecker(...)`. `None` means
    /// revocation checking is not configured for this trust manager (matches
    /// real-JDK's default: `PKIXParameters.isRevocationEnabled()` defaults to
    /// `true` for `CertPathValidator`, but SunJSSE's `TrustManagerFactory`
    /// path never attaches a revocation checker unless the caller explicitly
    /// builds one — see `validate_chain`'s "Step 7" doc for the enforcement
    /// semantics once this is `Some`).
    pub revocation: Option<RevocationConfig>,
}

/// Extracted `java.security.cert.PKIXRevocationChecker` configuration (see
/// `extract_revocation_checker` for how this is read off the real JDK
/// object). Drives `validate_chain`'s OCSP/CRL step.
#[derive(Clone, Debug, Default)]
pub struct RevocationConfig {
    /// `PKIXRevocationChecker.getOcspResponder()` — an explicit responder URI
    /// override. When `None`, the responder URL is read per-certificate from
    /// its Authority Information Access extension (OID 1.3.6.1.5.5.7.1.1,
    /// `id-ad-ocsp` access method).
    pub responder_uri: Option<String>,
    /// `PKIXRevocationChecker.getOcspResponderCert()` DER — an explicitly
    /// trusted OCSP responder certificate. When present, a `BasicOCSPResponse`
    /// signed by this exact cert is trusted directly (no further chain-to-CA
    /// check on the responder cert is required, matching real-JDK semantics
    /// for this option).
    pub responder_cert_der: Option<Vec<u8>>,
    /// `PKIXRevocationChecker.Option.ONLY_END_ENTITY` — only the leaf
    /// (end-entity) certificate is revocation-checked; CA certificates in the
    /// chain are skipped.
    pub only_end_entity: bool,
    /// `PKIXRevocationChecker.Option.PREFER_CRLS` — try CRL before OCSP
    /// (default is OCSP-first, CRL as fallback).
    pub prefer_crls: bool,
    /// `PKIXRevocationChecker.Option.NO_FALLBACK` — do not fall back to the
    /// other mechanism (CRL when OCSP is unavailable, or vice versa with
    /// `PREFER_CRLS`).
    pub no_fallback: bool,
    /// `PKIXRevocationChecker.Option.SOFT_FAIL` — treat "no answer obtainable"
    /// (network error, timeout, malformed response, responder-side error
    /// status, `unknown` cert status) as non-fatal and continue validation.
    /// A definite `revoked` answer is NEVER soft-failed, regardless of this
    /// flag — soft-fail only covers failure to *obtain* an answer.
    pub soft_fail: bool,
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

// FIX (tomcat-clientauth-engine-config): promoted from private to
// `pub(crate)` so `phases_late.rs`'s `KeyManagerFactory.getKeyManagers()`
// stub (a separate, competing registration on the PUBLIC
// `javax/net/ssl/KeyManagerFactory` class itself, active whenever the KMF
// object was allocated via that module's own `getInstance` rather than
// going through the real SPI `factorySpi.engineGetKeyManagers()`
// delegation chain this module's `kmf_engine_get_key_managers` backs) can
// register a KeyManager in the SAME registry `chooseClientAlias`/
// `getPrivateKey` (below) consult, instead of returning a non-functional,
// bare-interface-stamped `javax/net/ssl/X509KeyManager` object whose
// methods have no Code and throw `AbstractMethodError` the instant real
// Java bytecode (e.g. a test's wrapper `KeyManager` delegating to the
// array `getKeyManagers()` returned) calls one directly. Mirrors the
// identical `pub(crate)`-promotion precedent already applied to
// `jca::provider_chain::find`/`make_provider` for the sibling
// `KeyManagerFactory.getProvider()` fix (see this crate's
// `fixed-suite-bugs/tls-ocsp-clientcert-validation-not-enforced-FIXED.md`).
pub(crate) fn km_registry() -> &'static RwLock<HashMap<i32, KeyManagerState>> {
    KM_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

fn tm_registry() -> &'static RwLock<HashMap<i32, TrustManagerState>> {
    TM_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn next_km_id() -> i32 {
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
    /// Raw DER of the `signatureAlgorithm` AlgorithmIdentifier's `parameters`
    /// field — everything after the OID inside that SEQUENCE, empty when
    /// absent. Only RSASSA-PSS needs it: its digest, MGF digest and salt
    /// length live there and are NOT derivable from the OID
    /// ([`parse_rsa_pss_params`]).
    pub signature_algorithm_params: Vec<u8>,
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
    /// `rfc822Name` (email) entries from the SubjectAltName extension,
    /// lower-cased. Used for RFC 5280 §4.2.1.10 name-constraint matching.
    pub san_rfc822_names: Vec<String>,
    /// `uniformResourceIdentifier` entries from the SubjectAltName extension
    /// (original case preserved; the host portion is lower-cased when matched).
    pub san_uris: Vec<String>,
    /// `directoryName` entries from the SubjectAltName extension as the DER of
    /// each `Name` SEQUENCE. Used for directoryName name-constraint matching.
    pub san_dir_names: Vec<Vec<u8>>,
    /// Parsed `NameConstraints` extension (OID 2.5.29.30), present only on CA
    /// certificates that carry it. Drives RFC 5280 §4.2.1.10 enforcement of
    /// every subordinate certificate's names. `None` = no constraints imposed.
    pub name_constraints: Option<NameConstraints>,
    /// Raw big-endian bytes of `tbsCertificate.serialNumber` (the DER
    /// INTEGER content, minimal two's-complement encoding — i.e. exactly what
    /// `X509Certificate.getSerialNumber().toByteArray()` would return). Needed
    /// verbatim (not as an `i64`) for OCSP `CertID.serialNumber`, which must
    /// byte-match what the responder computed from the same certificate.
    pub serial_der: Vec<u8>,
    /// First `id-ad-ocsp` (OID 1.3.6.1.5.5.7.48.1) `accessLocation` URI found
    /// in the Authority Information Access extension (OID 1.3.6.1.5.5.7.1.1),
    /// if any. This is where `validate_chain`'s OCSP step sends the request
    /// when the active `RevocationConfig` has no explicit responder-URI
    /// override.
    pub ocsp_responder_uri: Option<String>,
}

/// RFC 5280 §4.2.1.10 `GeneralSubtrees`, split by the `GeneralName` types this
/// verifier evaluates (dNSName, rfc822Name, URI, iPAddress, directoryName).
/// Each vector holds the `base` of one `GeneralSubtree`; the rarely-used
/// `minimum`/`maximum` fields are ignored (RFC 5280 fixes `minimum = 0` and
/// forbids `maximum` for the PKIX profile). Subtree types this verifier does
/// not model (otherName, x400Address, ediPartyName, registeredID) are dropped
/// at parse time and therefore not enforced — see the module-level doc.
#[derive(Clone, Debug, Default)]
pub struct GeneralSubtrees {
    /// dNSName bases, lower-cased. A leading `.` (subdomain-only) is preserved
    /// and honoured by [`dns_constraint_matches`].
    pub dns: Vec<String>,
    /// rfc822Name (email) bases, lower-cased.
    pub email: Vec<String>,
    /// uniformResourceIdentifier bases, lower-cased (the host portion is what
    /// the constraint applies to, per RFC 5280 §4.2.1.10).
    pub uri: Vec<String>,
    /// iPAddress bases as `address || mask`: 8 bytes for IPv4 (4+4), 32 bytes
    /// for IPv6 (16+16).
    pub ip: Vec<Vec<u8>>,
    /// directoryName bases as the DER of each `Name` SEQUENCE.
    pub dir: Vec<Vec<u8>>,
}

/// Parsed `NameConstraints` extension (RFC 5280 §4.2.1.10):
/// `NameConstraints ::= SEQUENCE { permittedSubtrees [0] GeneralSubtrees OPTIONAL,
///                                 excludedSubtrees  [1] GeneralSubtrees OPTIONAL }`.
#[derive(Clone, Debug, Default)]
pub struct NameConstraints {
    pub permitted: GeneralSubtrees,
    pub excluded: GeneralSubtrees,
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
const TAG_ENUMERATED: u8 = 0x0a;
/// OCSP `CertStatus ::= CHOICE { ..., revoked [1] IMPLICIT RevokedInfo, ... }`
/// — `RevokedInfo` is a SEQUENCE, so this IMPLICIT tag is constructed
/// (context class 0x80 | constructed 0x20 | tag number 1).
const TAG_CONTEXT_1_CONSTRUCTED: u8 = 0xa1;
/// OCSP `CertStatus ::= CHOICE { ..., unknown [2] IMPLICIT UnknownInfo }` —
/// `UnknownInfo ::= NULL`, so this IMPLICIT tag is primitive (context class
/// 0x80 | tag number 2, no constructed bit).
const TAG_CONTEXT_2_PRIMITIVE: u8 = 0x82;

/// EKU OIDs we recognise. DER form (OID body, no tag/length).
///   id-kp-serverAuth: 1.3.6.1.5.5.7.3.1
pub const OID_KP_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
///   id-kp-clientAuth: 1.3.6.1.5.5.7.3.2
pub const OID_KP_CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];
///   id-kp-codeSigning: 1.3.6.1.5.5.7.3.3
pub const OID_KP_CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];
///   id-kp-emailProtection: 1.3.6.1.5.5.7.3.4
pub const OID_KP_EMAIL_PROTECTION: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x04];
///   id-kp-OCSPSigning: 1.3.6.1.5.5.7.3.9 — required EKU on a delegated OCSP
///   responder certificate (RFC 6960 §4.2.2.2).
pub const OID_KP_OCSP_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x09];

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
///   2.5.29.30 — NameConstraints
const OID_EXT_NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];
///   1.3.6.1.5.5.7.1.1 — AuthorityInfoAccess (RFC 5280 §4.2.2.1)
const OID_EXT_AUTHORITY_INFO_ACCESS: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x01];
///   1.3.6.1.5.5.7.48.1 — id-ad-ocsp `AccessDescription.accessMethod`
const OID_AD_OCSP: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01];

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

/// Additional `GeneralName` context-specific tags consumed for RFC 5280
/// §4.2.1.10 name-constraint evaluation (both inside SubjectAltName and as the
/// `base` of a `GeneralSubtree`). The dNSName/iPAddress tags above are reused.
///   [1] IMPLICIT IA5String — rfc822Name (email address)
const GN_TAG_RFC822: u8 = 0x81;
///   [6] IMPLICIT IA5String — uniformResourceIdentifier
const GN_TAG_URI: u8 = 0x86;
///   [4] EXPLICIT Name — directoryName (constructed: wraps a `Name` SEQUENCE)
const GN_TAG_DIRECTORY: u8 = 0xa4;

/// Signature-algorithm OIDs (the OID inside `tbsCertificate.signature` and
/// the outer `signatureAlgorithm`).
///   1.2.840.113549.1.1.11 — sha256WithRSAEncryption (PKCS#1 v1.5)
const OID_SIG_SHA256_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
///   1.2.840.10045.4.3.2 — ecdsa-with-SHA256 (P-256 most common)
const OID_SIG_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
///   1.2.840.113549.1.1.5 — sha1WithRSAEncryption
///
/// **Deliberate widening, recorded as such.** Accepting SHA-1 makes chains
/// that previously failed CLOSED verifiable. That is the HotSpot-parity
/// answer — HotSpot's `SunJSSE` validates these chains, and every JDK ships
/// `SHA1withRSA` — and it is what
/// `io.netty.handler.ssl.SslContextTrustManagerTest` (all 4 tests) needs: its
/// test CAs are SHA-1-signed, so CratonVM rejected them with
/// `signature-algorithm OID at index 0 not implemented`. SHA-1 is
/// collision-broken for *chosen-prefix* attacks against a CA that still signs
/// with it; this verifier's job is to agree with the platform it emulates, not
/// to impose a stricter policy the platform does not (a stricter policy that
/// only CratonVM enforces reads to an application as "this VM cannot do TLS").
const OID_SIG_SHA1_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
///   1.2.840.113549.1.1.12 — sha384WithRSAEncryption
const OID_SIG_SHA384_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
///   1.2.840.113549.1.1.13 — sha512WithRSAEncryption
const OID_SIG_SHA512_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
///   1.2.840.10045.4.3.3 — ecdsa-with-SHA384
const OID_SIG_ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
///   1.2.840.10045.4.3.4 — ecdsa-with-SHA512
const OID_SIG_ECDSA_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
///   1.2.840.10045.4.3.1 — ecdsa-with-SHA224 (recognised, see below)
const OID_SIG_ECDSA_SHA224: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x01];
///   1.2.840.113549.1.1.14 — sha224WithRSAEncryption (recognised, see below)
const OID_SIG_SHA224_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0e];

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
//   * 1.2.840.113549.1.1.14 / 1.2.840.10045.4.3.1 — the SHA-224 pair. Named
//     here rather than left to the catch-all so the error says "known and
//     unimplemented"; SHA-224 is the one member of the SHA-2 family this tree
//     has no engine for, and no CA in the corpus issues with it.
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
    let signature_algorithm_params = sig_alg_oid.rest.to_vec();

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
    let serial_der = serial.content.to_vec();
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
    let mut san_rfc822_names: Vec<String> = Vec::new();
    let mut san_uris: Vec<String> = Vec::new();
    let mut san_dir_names: Vec<Vec<u8>> = Vec::new();
    let mut name_constraints: Option<NameConstraints> = None;
    let mut ocsp_responder_uri: Option<String> = None;

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
                                GN_TAG_RFC822 => {
                                    if let Ok(s) = std::str::from_utf8(gn.content) {
                                        san_rfc822_names.push(s.to_ascii_lowercase());
                                    }
                                }
                                GN_TAG_URI => {
                                    if let Ok(s) = std::str::from_utf8(gn.content) {
                                        san_uris.push(s.to_string());
                                    }
                                }
                                GN_TAG_DIRECTORY => {
                                    // [4] EXPLICIT Name — gn.content is the
                                    // wrapped `Name` SEQUENCE DER.
                                    if let Ok(name) = read_tlv_tagged(gn.content, TAG_SEQUENCE) {
                                        san_dir_names.push(name.full.to_vec());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                } else if oid.content == OID_EXT_NAME_CONSTRAINTS {
                    // RFC 5280 §4.2.1.10. A structurally-malformed constraints
                    // extension is dropped (treated as "no constraints") rather
                    // than aborting the whole parse — consistent with the
                    // lenient SAN handling above. The CA practically never ships
                    // a malformed NameConstraints, and the chain still gets the
                    // signature/clock/BC checks.
                    if let Ok(nc) = parse_name_constraints(value_tlv.content) {
                        name_constraints = Some(nc);
                    }
                } else if oid.content == OID_EXT_AUTHORITY_INFO_ACCESS {
                    // AuthorityInfoAccessSyntax ::= SEQUENCE SIZE (1..MAX) OF
                    //   AccessDescription
                    // AccessDescription ::= SEQUENCE { accessMethod OID,
                    //   accessLocation GeneralName }
                    // We want the first `accessLocation` whose `accessMethod`
                    // is id-ad-ocsp (1.3.6.1.5.5.7.48.1) and whose
                    // `accessLocation` is a uniformResourceIdentifier ([6]).
                    // A malformed AIA is non-fatal, same lenient treatment as
                    // SubjectAltName above.
                    if let Ok(seq) = read_tlv_tagged(value_tlv.content, TAG_SEQUENCE) {
                        let mut ac = seq.content;
                        while !ac.is_empty() {
                            let ad = match read_tlv_tagged(ac, TAG_SEQUENCE) {
                                Ok(t) => t,
                                Err(_) => break,
                            };
                            ac = ad.rest;
                            let method = match read_tlv_tagged(ad.content, TAG_OID) {
                                Ok(t) => t,
                                Err(_) => continue,
                            };
                            if method.content == OID_AD_OCSP {
                                if let Ok(loc) = read_tlv(method.rest) {
                                    if loc.tag == GN_TAG_URI {
                                        if let Ok(s) = std::str::from_utf8(loc.content) {
                                            if ocsp_responder_uri.is_none() {
                                                ocsp_responder_uri = Some(s.to_string());
                                            }
                                        }
                                    }
                                }
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
        signature_algorithm_params,
        signature_value,
        is_v3,
        san_dns_names,
        san_ip_addresses,
        subject_cn,
        san_rfc822_names,
        san_uris,
        san_dir_names,
        name_constraints,
        serial_der,
        ocsp_responder_uri,
    })
}

/// Parse a `NameConstraints` extension value (the bytes inside the extension's
/// OCTET STRING):
/// `NameConstraints ::= SEQUENCE { permittedSubtrees [0] GeneralSubtrees OPTIONAL,
///                                 excludedSubtrees  [1] GeneralSubtrees OPTIONAL }`.
/// Both subtree fields are IMPLICIT-tagged, so the `[0]`/`[1]` constructed tag
/// directly wraps the `SEQUENCE OF GeneralSubtree`.
fn parse_name_constraints(value: &[u8]) -> Result<NameConstraints, CertParseError> {
    let seq = read_tlv_tagged(value, TAG_SEQUENCE)?;
    let mut nc = NameConstraints::default();
    let mut c = seq.content;
    while !c.is_empty() {
        let t = read_tlv(c)?;
        c = t.rest;
        match t.tag {
            0xa0 => nc.permitted = parse_general_subtrees(t.content)?, // [0] permittedSubtrees
            0xa1 => nc.excluded = parse_general_subtrees(t.content)?,  // [1] excludedSubtrees
            _ => {}
        }
    }
    Ok(nc)
}

/// Parse `GeneralSubtrees ::= SEQUENCE SIZE (1..MAX) OF GeneralSubtree`. The
/// input is the *content* of the IMPLICIT `[0]`/`[1]` tag, i.e. the
/// concatenation of `GeneralSubtree` SEQUENCE elements.
///
/// `GeneralSubtree ::= SEQUENCE { base GeneralName, minimum [0] DEFAULT 0,
///                                maximum [1] OPTIONAL }`. We read the `base`
/// (first element) and ignore `minimum`/`maximum` (RFC 5280 §4.2.1.10 mandates
/// `minimum = 0` and absent `maximum` in the PKIX profile).
fn parse_general_subtrees(input: &[u8]) -> Result<GeneralSubtrees, CertParseError> {
    let mut out = GeneralSubtrees::default();
    let mut c = input;
    while !c.is_empty() {
        let sub = read_tlv_tagged(c, TAG_SEQUENCE)?;
        c = sub.rest;
        let base = read_tlv(sub.content)?;
        match base.tag {
            SAN_TAG_DNS_NAME => {
                if let Ok(s) = std::str::from_utf8(base.content) {
                    out.dns.push(s.to_ascii_lowercase());
                }
            }
            GN_TAG_RFC822 => {
                if let Ok(s) = std::str::from_utf8(base.content) {
                    out.email.push(s.to_ascii_lowercase());
                }
            }
            GN_TAG_URI => {
                if let Ok(s) = std::str::from_utf8(base.content) {
                    out.uri.push(s.to_ascii_lowercase());
                }
            }
            SAN_TAG_IP_ADDRESS => {
                // address || mask: 8 bytes (IPv4) or 32 bytes (IPv6).
                if base.content.len() == 8 || base.content.len() == 32 {
                    out.ip.push(base.content.to_vec());
                }
            }
            GN_TAG_DIRECTORY => {
                // [4] EXPLICIT Name — base.content wraps the `Name` SEQUENCE.
                if let Ok(name) = read_tlv_tagged(base.content, TAG_SEQUENCE) {
                    out.dir.push(name.full.to_vec());
                }
            }
            _ => { /* unsupported GeneralName type — not modelled (see doc) */ }
        }
    }
    Ok(out)
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
/// True if the cert is *preferred* as a **server** certificate.
///
/// **This is a preference, not an eligibility test.** JSSE's default
/// `KeyManagerFactory` algorithm is `SunX509`, and
/// `SunX509KeyManagerImpl.getAliases` filters ONLY on the key's algorithm and
/// (when the peer supplies one) the issuer list — it never consults KeyUsage or
/// ExtendedKeyUsage. `X509KeyManagerImpl` (NewSunX509) does look at them, but it
/// *ranks* on the result and still answers; an `EXTENSION_MISMATCH` alias sorts
/// last rather than dropping out. Measured on JDK 25 with a self-signed cert
/// carrying `KeyUsage=digitalSignature` and `EKU={id-kp-serverAuth}` only:
///
/// ```text
/// SunX509     getClientAliases(RSA)=[key]      chooseClientAlias(RSA)=key
/// NewSunX509  getClientAliases(RSA)=[1.0.key]  chooseClientAlias(RSA)=3.0.key
/// ```
///
/// So callers must use these to ORDER the by-key-type alias lists, never to
/// decide membership. Using them as a filter is what made
/// `chooseClientAlias(RSA)` answer `null` here for exactly that certificate,
/// and a client that has no alias sends no certificate: against a server with
/// `ClientAuth.REQUIRE` that is `SSLV3_ALERT_HANDSHAKE_FAILURE` /
/// `TLSV1_ALERT_CERTIFICATE_REQUIRED` — the 14 residual rows of
/// `JdkDelegatingPrivateKeyMethodTest`, whose fixture builds its cert with
/// `.setKeyUsage(true, digitalSignature).addExtendedKeyUsageServerAuth()` and
/// then uses it on BOTH sides.
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

/// True if the cert is *preferred* as a **client** certificate. See
/// [`is_server_cert`] for why this must not be used as an eligibility filter.
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

/// Java's `String.hashCode()`: a 31-multiplier polynomial hash over UTF-16
/// code units. Frozen by the `String` serialization contract — stable across
/// every JDK version.
fn java_string_hash_code(s: &str) -> i32 {
    let mut h: i32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(unit as i32);
    }
    h
}

/// `java.util.HashMap`'s bucket-index derivation: `hash(key) ^ (hash(key)
/// >>> 16)`, masked to the table's (power-of-two) capacity.
fn java_hashmap_bucket(key: &str, capacity: usize) -> usize {
    let h = java_string_hash_code(key) as u32;
    let spread = h ^ (h >> 16);
    (spread as usize) & (capacity - 1)
}

/// The table capacity a `new HashMap<>()` (default initial capacity 16, load
/// factor 0.75) would have after inserting `n` entries — doubling whenever
/// size exceeds `capacity * 0.75`, exactly like `HashMap.resize()`.
fn java_hashmap_capacity_for(n: usize) -> usize {
    let mut capacity = 16usize;
    let mut threshold = 12usize;
    while n > threshold {
        capacity *= 2;
        threshold = (capacity * 3) / 4;
    }
    capacity
}

/// Reorder `keys` (given in some other, e.g. keystore-file, order) into the
/// order a real `java.util.HashMap<String, V>` would yield them in when
/// iterated after inserting them in that same original order.
///
/// This exists to bug-compatibly match `sun.security.ssl.SunX509KeyManagerImpl`
/// (the JDK's default `SunX509`-algorithm `KeyManager`, what
/// `KeyManagerFactory.getDefaultAlgorithm()` names): it loads every keystore
/// alias into a plain `HashMap<String,X509Credentials> credentialsMap`, and
/// `getClientAliases()`/`getServerAliases()`/`chooseClientAlias()` all derive
/// their candidate order from iterating *that* map — bucket-index order, not
/// keystore/file order. When a keystore has two otherwise-equally-eligible
/// client identities (same key type, same validity, no EKU to disambiguate —
/// e.g. Spring Boot's own `NettyReactiveWebServerFactoryTests` PKCS12 test
/// fixture, which carries a "spring-boot" and a "test-alias" client identity
/// side by side, only one of which the test's server trusts), which one
/// `chooseClientAlias()` returns is entirely this HashMap-bucket accident —
/// and real HotSpot's answer for THIS keystore's alias strings is
/// deterministic (Java's `String.hashCode()`/`HashMap` bucketing are frozen,
/// unsalted algorithms), so replicating it exactly is the only way to match
/// observable behavior rather than picking whichever candidate happens to be
/// physically first in the keystore file.
fn java_hashmap_iteration_order(keys: &[String]) -> Vec<String> {
    let capacity = java_hashmap_capacity_for(keys.len());
    let mut indexed: Vec<(usize, usize, &String)> = keys
        .iter()
        .enumerate()
        .map(|(insertion_index, key)| (java_hashmap_bucket(key, capacity), insertion_index, key))
        .collect();
    // Ascending bucket index; entries within the same bucket keep their
    // original (insertion) order, matching Java 8+ HashMap's tail-append
    // collision chaining.
    indexed.sort_by_key(|(bucket, insertion_index, _)| (*bucket, *insertion_index));
    indexed.into_iter().map(|(_, _, key)| key.clone()).collect()
}

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

    // First pass: collect every PrivateKeyEntry alias in keystore/file order
    // (same order real `KeyStore.aliases()` would enumerate them), computing
    // everything needed to classify it — but not yet deciding candidate
    // order for the by-key-type lists below.
    struct PrivateKeyAlias<'a> {
        alias: &'a str,
        key_type: String,
        is_server: bool,
        is_client: bool,
    }
    let mut private_key_aliases = Vec::new();
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
            private_key_aliases.push(PrivateKeyAlias {
                alias,
                key_type,
                is_server: is_server_cert(&leaf),
                is_client: is_client_cert(&leaf),
            });
        }
    }

    // Second pass: push into the by-key-type candidate lists in
    // `SunX509KeyManagerImpl`'s `credentialsMap` HashMap-bucket order rather
    // than keystore/file order — see `java_hashmap_iteration_order`'s doc
    // comment.
    let ordered_aliases = java_hashmap_iteration_order(
        &private_key_aliases
            .iter()
            .map(|a| a.alias.to_string())
            .collect::<Vec<_>>(),
    );
    // `is_server`/`is_client` ORDER these lists; they do not gate them. Every
    // alias with a matching key type is a candidate for both roles, exactly as
    // `SunX509KeyManagerImpl` (the default) treats it — see `is_server_cert`.
    // Two passes so a properly-marked cert still wins when a keystore holds
    // several.
    for preferred in [true, false] {
        for alias in &ordered_aliases {
            let entry = private_key_aliases
                .iter()
                .find(|a| a.alias == alias)
                .expect("alias came from private_key_aliases");
            if entry.is_server == preferred {
                state
                    .server_aliases_by_key_type
                    .entry(entry.key_type.clone())
                    .or_default()
                    .push(alias.clone());
            }
            if entry.is_client == preferred {
                state
                    .client_aliases_by_key_type
                    .entry(entry.key_type.clone())
                    .or_default()
                    .push(alias.clone());
            }
        }
    }
    state
}

/// Build a `KeyManagerState` by ENUMERATING a live Java `KeyStore` object
/// through its own bytecode, instead of reading this crate's native keystore
/// registry.
///
/// Why a second path exists: `build_key_manager_state` can only see stores
/// CratonVM itself created, and an application is free to hand
/// `KeyManagerFactory.init` a `KeyStore` of its own — netty's
/// `OpenSslX509KeyManagerFactory.newKeyless` does exactly that, with a private
/// `KeyStore` subclass over a hand-written `KeyStoreSpi` whose entries are
/// keyless certificate chains. `keystore_id_from_object` answers 0 for such a
/// store, and the `getKeyManagers()` fallback for that case used to be an
/// object stamped with the bare `javax/net/ssl/X509KeyManager` INTERFACE id,
/// every method of which is abstract: netty's
/// `OpenSslKeyMaterialProvider.chooseKeyMaterial` called
/// `getCertificateChain(alias)` on it and died with `AbstractMethodError`,
/// taking out all 27 of `JdkDelegatingPrivateKeyMethodTest` and all 24 of
/// `OpenSslPrivateKeyMethodTest`.
///
/// Enumerating the store through `aliases()` / `getCertificateChain()` /
/// `getKey()` is what the real `SunX509KeyManagerImpl` constructor does, and it
/// works for ANY `KeyStore` implementation rather than only for the shapes this
/// VM knows how to build natively.
///
/// The private key is kept BY REFERENCE as well as by bytes — see
/// [`KeyManagerState::aliases_to_live_key`] for why bytes alone cannot serve an
/// opaque key.
pub(crate) fn build_key_manager_state_from_live_keystore(
    ctx: &mut dyn NativeContext,
    ks: ObjectRef,
    password: Option<ObjectRef>,
) -> KeyManagerState {
    let mut state = KeyManagerState::default();
    let base = ctx.pin_native_root(ks);
    let pw_pin = password.map(|p| ctx.pin_native_root(p));

    // 1. aliases() — bounded, so a misbehaving Enumeration cannot wedge init.
    let mut aliases: Vec<String> = Vec::new();
    let ks_now = ctx.read_native_pin(base, ks);
    if let Ok(Some(Value::Object(Some(en)))) =
        ctx.invoke_virtual(ks_now, "aliases", "()Ljava/util/Enumeration;", &[])
    {
        let en_pin = ctx.pin_native_root(en);
        for _ in 0..4096 {
            let en_now = ctx.read_native_pin(en_pin, en);
            match ctx.invoke_virtual(en_now, "hasMoreElements", "()Z", &[]) {
                Ok(Some(Value::Int(1))) => {}
                _ => break,
            }
            let en_now = ctx.read_native_pin(en_pin, en);
            match ctx.invoke_virtual(en_now, "nextElement", "()Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => match ctx.read_string(s) {
                    Some(a) => aliases.push(a),
                    None => break,
                },
                _ => break,
            }
        }
    }

    // 2. Per alias: the chain (DER) and the key (bytes AND reference).
    struct Candidate {
        alias: String,
        key_type: String,
        is_server: bool,
        is_client: bool,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut live_pins: Vec<(String, usize, ObjectRef)> = Vec::new();
    for alias in &aliases {
        let a_str = ctx.create_string(alias);
        let ks_now = ctx.read_native_pin(base, ks);
        let chain: Vec<Vec<u8>> = match ctx.invoke_virtual(
            ks_now,
            "getCertificateChain",
            "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
            &[Value::Object(Some(a_str))],
        ) {
            Ok(Some(Value::Object(Some(arr)))) => {
                let n = ctx.array_length(arr);
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    if let Value::Object(Some(c)) = ctx.get_array_element(arr, i) {
                        let der = crate::keystore::certificate_der(ctx, c);
                        if !der.is_empty() {
                            v.push(der);
                        }
                    }
                }
                v
            }
            _ => Vec::new(),
        };
        if chain.is_empty() {
            continue;
        }
        let leaf = match parse_certificate(&chain[0]) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let key_type = classify_key_type(&leaf.spki_algorithm_oid).to_string();
        let is_server = is_server_cert(&leaf);
        let is_client = is_client_cert(&leaf);

        let a_str2 = ctx.create_string(alias);
        let ks_now = ctx.read_native_pin(base, ks);
        let pw_val = match (pw_pin, password) {
            (Some(p), Some(orig)) => Value::Object(Some(ctx.read_native_pin(p, orig))),
            _ => Value::Object(None),
        };
        if let Ok(Some(Value::Object(Some(k)))) = ctx.invoke_virtual(
            ks_now,
            "getKey",
            "(Ljava/lang/String;[C)Ljava/security/Key;",
            &[Value::Object(Some(a_str2)), pw_val],
        ) {
            let der = crate::keystore::read_encoded_byte_array(ctx, k);
            if !der.is_empty() {
                state.aliases_to_key.insert(alias.clone(), der);
            }
            live_pins.push((alias.clone(), ctx.pin_native_root(k), k));
        }
        state.aliases_to_chain.insert(alias.clone(), chain);
        candidates.push(Candidate {
            alias: alias.clone(),
            key_type,
            is_server,
            is_client,
        });
    }

    // 3. Candidate order, matching `build_key_manager_state`'s second pass.
    let ordered = java_hashmap_iteration_order(
        &candidates
            .iter()
            .map(|c| c.alias.clone())
            .collect::<Vec<_>>(),
    );
    // Preference, not eligibility — same two-pass shape as
    // `build_key_manager_state`; see `is_server_cert`.
    for preferred in [true, false] {
        for alias in &ordered {
            let Some(c) = candidates.iter().find(|c| &c.alias == alias) else {
                continue;
            };
            if c.is_server == preferred {
                state
                    .server_aliases_by_key_type
                    .entry(c.key_type.clone())
                    .or_default()
                    .push(alias.clone());
            }
            if c.is_client == preferred {
                state
                    .client_aliases_by_key_type
                    .entry(c.key_type.clone())
                    .or_default()
                    .push(alias.clone());
            }
        }
    }

    // 4. Re-read every live key through its pin — the allocations above may
    // have moved them — then release the whole pin frame.
    for (alias, pin, orig) in &live_pins {
        let now = ctx.read_native_pin(*pin, *orig);
        state.aliases_to_live_key.insert(alias.clone(), now);
    }
    ctx.unpin_native_roots(base);
    state
}

/// Build a `TrustManagerState` from either a caller-supplied `LoadedKeyStore`
/// or the system trust store. Non-zero `keystore_id` means an explicit
/// truststore was configured, so it is restrictive: platform roots are not
/// appended as fallback.
pub fn build_trust_manager_state(keystore_id: i32) -> TrustManagerState {
    let mut state = TrustManagerState {
        keystore_id,
        ..Default::default()
    };

    let explicit_truststore = keystore_id != 0;

    // (1) User-supplied trust anchors out of the bound keystore.
    if explicit_truststore {
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
                    keystore::EntryKind::SecretKey { .. } => continue,
                };
                insert_anchor(&mut state, der);
            }
        }
        return state;
    }

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
                keystore::EntryKind::SecretKey { .. } => continue,
            };
            insert_anchor(&mut state, der);
        }
    }

    // (2) System trust store via rustls-native-certs for the default manager.
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

/// Register a fully-built `TrustManagerState` under a fresh id in the shared
/// `tm_registry`. Every `TrustManagerFactory`-SPI path that produces a
/// `TrustManager` object (`tmf_engine_init`/`tmf_engine_init_params` below,
/// and `tls.rs`'s SimpleFactory-style `getTrustManagers()`) MUST go through
/// this instead of stamping a raw KeyStore registry id onto
/// `cratonvm$x509tm$id` directly. Both `next_tm_id()` (this module) and
/// `keystore::keystore_register`'s counter (a DIFFERENT registry) start at 1
/// and increment independently — stamping either kind of id onto the same
/// field, ambiguously, meant `validate_cert_chain`
/// (`tls.rs::checkClientTrusted`/`checkServerTrusted`) could resolve a real,
/// correctly-populated `TrustManagerState` (e.g. one built from
/// `CertPathTrustManagerParameters`/OCSP config, which has no backing
/// keystore at all) against a numerically-coincident but UNRELATED keystore
/// — or, far more commonly, against NO keystore at all, silently emptying
/// the trust-anchor set for a fully valid PKIX-configured connector (see
/// `trust_manager_state_by_id`'s doc for the read side of this fix).
pub(crate) fn register_trust_manager_state(state: TrustManagerState) -> i32 {
    let id = next_tm_id();
    tm_registry().write().insert(id, state);
    id
}

/// Resolve a `cratonvm$x509tm$id` field value to its `TrustManagerState`.
/// Checks `tm_registry` FIRST — the id space every `TrustManagerFactory` SPI
/// path here now populates via `register_trust_manager_state` — and only
/// falls back to treating `id` as a raw KeyStore registry id (matching
/// `build_trust_manager_state`'s historical contract) when nothing is
/// registered under it: id 0 (no `init()`/default trust store), or a caller
/// that stamped a keystore id directly without going through
/// `register_trust_manager_state`.
pub(crate) fn trust_manager_state_by_id(id: i32) -> TrustManagerState {
    if id != 0 {
        if let Some(state) = tm_registry().read().get(&id).cloned() {
            return state;
        }
    }
    build_trust_manager_state(id)
}

/// The two shapes that reached this validator only once the client started
/// capturing whole chains: a CROSS-SIGNED root, and a P-384 issuer key.
///
/// Both are built with real OpenSSL keys and real signatures rather than the
/// synthetic `mk_cert` fixtures beside them, because both are questions about
/// CRYPTOGRAPHY and about identity across two encodings of one key — neither
/// survives a fixture whose signatures are not real.
#[cfg(all(test, unix))]
mod real_chain_shape_tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::{BigNum, MsbOption};
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::nid::Nid;
    use openssl::pkey::{PKey, Private};
    use openssl::x509::extension::BasicConstraints;
    use openssl::x509::{X509Name, X509};

    fn serial() -> openssl::asn1::Asn1Integer {
        let mut bn = BigNum::new().expect("bn");
        bn.rand(64, MsbOption::MAYBE_ZERO, false).expect("rand");
        bn.to_asn1_integer().expect("serial")
    }

    fn name(cn: &str) -> X509Name {
        let mut n = X509Name::builder().expect("name builder");
        n.append_entry_by_text("CN", cn).expect("cn");
        n.build()
    }

    fn p384_key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::SECP384R1).expect("group");
        PKey::from_ec_key(EcKey::generate(&group).expect("keygen")).expect("pkey")
    }

    /// A certificate for `subject`/`key`, signed by `(issuer_name, issuer_key)`,
    /// CA or leaf.
    fn cert(
        subject: &str,
        key: &PKey<Private>,
        issuer: &str,
        issuer_key: &PKey<Private>,
        ca: bool,
    ) -> Vec<u8> {
        let mut b = X509::builder().expect("builder");
        b.set_version(2).expect("v3");
        b.set_serial_number(&serial()).expect("serial");
        b.set_subject_name(&name(subject)).expect("subject");
        b.set_issuer_name(&name(issuer)).expect("issuer");
        b.set_pubkey(key).expect("pubkey");
        b.set_not_before(&Asn1Time::days_from_now(0).expect("nb"))
            .expect("nb");
        b.set_not_after(&Asn1Time::days_from_now(3650).expect("na"))
            .expect("na");
        let bc = if ca {
            BasicConstraints::new().critical().ca().build()
        } else {
            BasicConstraints::new().critical().build()
        };
        b.append_extension(bc.expect("bc")).expect("bc ext");
        b.sign(issuer_key, MessageDigest::sha384()).expect("sign");
        b.build().to_der().expect("der")
    }

    fn trust_with(anchor_der: Vec<u8>) -> TrustManagerState {
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, anchor_der);
        trust
    }

    /// A peer that ends its chain with a CROSS-SIGNED copy of a root that IS in
    /// the trust store — `www.cloudflare.com` and `adoptium.net` both serve
    /// `CN=GTS Root R4` as signed by `CN=GlobalSign Root CA`, whose bytes
    /// differ from the self-signed GTS Root R4 in JDK 25's cacerts and whose
    /// own issuer that cacerts no longer ships.
    ///
    /// What carries it is the PATH REBUILD, not the anchor comparison: the
    /// cross-signed tail is dropped by `select_path` and the anchor is found
    /// by issuer lookup. Named that way because the first version of this test
    /// was written to guard a relaxation of `presented_cert_is_anchor` and
    /// passed just as well with that relaxation reverted — it never touched
    /// it. Breaking `rebuild_path` is what fails this.
    #[test]
    fn a_cross_signed_tail_is_dropped_and_the_real_anchor_still_found() {
        let root_key = p384_key();
        let other_root_key = p384_key();
        let leaf_key = p384_key();

        let root_self_signed = cert("Trusted Root", &root_key, "Trusted Root", &root_key, true);
        // The SAME subject and the SAME key, certified by somebody else — the
        // shape a real cross-certificate has.
        let root_cross_signed = cert("Trusted Root", &root_key, "Other Root", &other_root_key, true);
        assert_ne!(
            root_self_signed, root_cross_signed,
            "the fixture must present a DIFFERENT encoding, or it proves nothing"
        );
        let leaf = cert("leaf.example", &leaf_key, "Trusted Root", &root_key, false);

        let trust = trust_with(root_self_signed);
        // Only the cross-signed copy is on the wire; the issuer that signed it
        // is NOT in the trust store, exactly as with GlobalSign Root CA.
        let chain = vec![leaf, root_cross_signed];
        assert!(
            validate_chain(&chain, &trust).is_ok(),
            "the cross-signed tail must be dropped and the real anchor still found"
        );
    }

    /// …and the same subject with a DIFFERENT key must still be refused, or
    /// the relaxation above would be a hole rather than a fix.
    #[test]
    fn the_same_subject_with_a_different_key_is_not_that_root() {
        let root_key = p384_key();
        let impostor_key = p384_key();
        let other_root_key = p384_key();
        let leaf_key = p384_key();

        let root_self_signed = cert("Trusted Root", &root_key, "Trusted Root", &root_key, true);
        let impostor = cert(
            "Trusted Root",
            &impostor_key,
            "Other Root",
            &other_root_key,
            true,
        );
        let leaf = cert("leaf.example", &leaf_key, "Trusted Root", &impostor_key, false);

        let trust = trust_with(root_self_signed);
        assert!(
            validate_chain(&vec![leaf, impostor], &trust).is_err(),
            "a certificate that only borrows the anchor's NAME must be refused"
        );
    }

    /// The whole chain signed by P-384 keys. Before the named-curve verifier
    /// this failed with `BadSignature` at whichever index first had a P-384
    /// issuer — six of twenty live public sites.
    #[test]
    fn a_p384_chain_validates() {
        let root_key = p384_key();
        let inter_key = p384_key();
        let leaf_key = p384_key();
        let root = cert("P384 Root", &root_key, "P384 Root", &root_key, true);
        let inter = cert("P384 Intermediate", &inter_key, "P384 Root", &root_key, true);
        let leaf = cert("leaf.example", &leaf_key, "P384 Intermediate", &inter_key, false);

        let trust = trust_with(root);
        assert!(
            validate_chain(&vec![leaf.clone(), inter.clone()], &trust).is_ok(),
            "a P-384 chain to a P-384 anchor must validate"
        );

        // The paired refusal: one flipped byte in the leaf's signature must
        // make it fail, or "validates" above would also hold for a verifier
        // that never checks anything.
        let mut tampered = leaf;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(
            validate_chain(&vec![tampered, inter], &trust).is_err(),
            "a tampered P-384 signature must be refused"
        );
    }
}

fn insert_anchor(state: &mut TrustManagerState, der: Vec<u8>) {
    let parsed = match parse_certificate(&der) {
        Ok(p) => p,
        Err(_) => return,
    };
    state.anchor_ders.push(der.clone());
    state
        .anchors
        .entry(parsed.subject_der.clone())
        .or_default()
        .push(AnchorInfo {
            subject_der: parsed.subject_der,
            spki_der: parsed.spki_der,
            full_cert_der: Some(der),
        });
}

fn presented_cert_is_anchor(
    anchor: &AnchorInfo,
    presented_der: &[u8],
    parsed: &ParsedCert,
) -> bool {
    // Exact-encoding equality, NOT the (name, key) pair RFC 5280 §6.1.1
    // defines a trust anchor by. That relaxation was written, and then
    // MEASURED to fix nothing, so it is not here.
    //
    // The shape it was aimed at is real: `www.cloudflare.com` and
    // `adoptium.net` both end their chain with `CN=GTS Root R4` as signed by
    // `CN=GlobalSign Root CA`, whose bytes differ from the self-signed GTS
    // Root R4 in JDK 25's cacerts, and whose own issuer that cacerts no longer
    // ships. Both were rejected with `NoTrustAnchor` — which is what this
    // exact-match produces, and is why it looked like the cause.
    //
    // It was not. `validate_chain` retries through `rebuild_path`, and
    // `select_path` stops the moment the current certificate's ISSUER is a
    // configured anchor — so the cross-signed tail is dropped and the anchor
    // is found by issuer lookup instead. The presented-order `NoTrustAnchor`
    // is simply the error `validate_chain` reports when the REBUILD also
    // fails, and at the time it failed for an unrelated reason: the P-384
    // ECDSA gap. With that closed, both sites validate with this function
    // untouched — 20 of 20 live public sites, measured with the relaxation
    // reverted.
    //
    // Read an error message as a symptom, not as an attribution: the one
    // printed here came from the arm that ran FIRST, not from the arm that
    // decided.
    match anchor.full_cert_der.as_deref() {
        Some(anchor_der) => anchor_der == presented_der,
        None => anchor.subject_der == parsed.subject_der && anchor.spki_der == parsed.spki_der,
    }
}

/// Is this exact presented certificate one the application installed as a
/// trust anchor? Used to keep the anchor out of the path-validation steps that
/// RFC 5280 §6.1 applies only to the certificates ON the path.
fn cert_is_stored_anchor(trust: &TrustManagerState, der: &[u8], parsed: &ParsedCert) -> bool {
    anchors_for_subject(trust, &parsed.subject_der)
        .iter()
        .any(|a| presented_cert_is_anchor(a, der, parsed))
}

fn anchors_for_subject<'a>(trust: &'a TrustManagerState, subject_der: &[u8]) -> &'a [AnchorInfo] {
    trust
        .anchors
        .get(subject_der)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Order the presented certificates into a PATH from the end entity, dropping
/// any that are not on it. Returns the indices in path order, starting at 0.
///
/// `validate_chain` used to require `parsed[i].issuer == parsed[i+1].subject`
/// for every `i` and reject anything else as `BrokenChain`. That is path
/// VALIDATION of an already-built path; what a caller hands
/// `checkClientTrusted`/`checkServerTrusted` is a certificate SET, and RFC 5280
/// §6 is explicit that building the path from it comes first. Two shapes that
/// are legal and were refused:
///
/// * a **cross-signed** intermediate — the same subject and key certified by
///   two different roots, both intermediates supplied so that either root can
///   be the trust anchor. Only one of them is on the path to any given anchor;
///   the other is not a break, it is an alternative. This is
///   `io.netty.pkitesting.CertificateBuilderTest.authenticatingCrossSignedCertificate`,
///   which supplies `[leaf, crossIssuer, oldIssuer]` (both issuers named
///   `CN=issuer.netty.io`) and trusts only the root that signed `oldIssuer`;
/// * a chain sent out of order, which real peers do send.
///
/// Selection is deliberately conservative: it only ever REORDERS and DROPS. It
/// never admits a certificate that the steps after it would have rejected —
/// every cert on the returned path still goes through the CA, anchor, name
/// constraint, signature and revocation steps unchanged. Dropping an
/// off-path certificate is what the JDK's own `SunCertPathBuilder` does.
///
/// Where several candidates share the required subject DN, the one whose own
/// issuer is a configured trust anchor wins, then one that is itself an anchor
/// subject; ties fall back to the caller's order. That preference is the whole
/// of the cross-signing fix — both candidates link to the leaf, and only the
/// anchor test tells them apart.
fn select_path(parsed: &[ParsedCert], trust: &TrustManagerState) -> Vec<usize> {
    let mut path = vec![0usize];
    let mut used = vec![false; parsed.len()];
    used[0] = true;
    // Bounded by the input length: a cross-certified pair is a cycle in the
    // subject/issuer graph, and `used` is what keeps it from being walked
    // forever.
    for _ in 1..parsed.len() {
        let cur = &parsed[*path.last().expect("path is never empty")];
        // Self-issued root: nothing can follow it.
        if cur.issuer_der == cur.subject_der {
            break;
        }
        // The path ends as soon as the current certificate's issuer is a
        // configured anchor — continuing past it would prefer a longer path
        // over the trusted one.
        if !anchors_for_subject(trust, &cur.issuer_der).is_empty() {
            break;
        }
        let mut best: Option<(u8, usize)> = None;
        for (j, cand) in parsed.iter().enumerate() {
            if used[j] || cand.subject_der != cur.issuer_der {
                continue;
            }
            let rank = if !anchors_for_subject(trust, &cand.issuer_der).is_empty() {
                0
            } else if !anchors_for_subject(trust, &cand.subject_der).is_empty() {
                1
            } else {
                2
            };
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, j));
            }
        }
        match best {
            Some((_, j)) => {
                used[j] = true;
                path.push(j);
            }
            None => break,
        }
    }
    path
}

fn select_trust_anchor<'a>(
    parsed: &'a [ParsedCert],
    chain: &[Vec<u8>],
    trust: &'a TrustManagerState,
) -> Result<(&'a AnchorInfo, bool, bool), TrustError> {
    let last_idx = parsed.len() - 1;
    let last = &parsed[last_idx];
    let last_der = &chain[last_idx];

    for anchor in anchors_for_subject(trust, &last.subject_der) {
        if presented_cert_is_anchor(anchor, last_der, last) {
            return Ok((anchor, true, false));
        }
    }

    let issuer_anchors = anchors_for_subject(trust, &last.issuer_der);
    if issuer_anchors.is_empty() {
        return Err(TrustError::NoTrustAnchor);
    }
    if last.signature_value.is_empty() {
        return Err(TrustError::SignatureFailed { at: last_idx });
    }

    let mut first_error = None;
    for anchor in issuer_anchors {
        match verify_one_signature(last_idx, last, anchor.spki_der.as_slice()) {
            Ok(()) => return Ok((anchor, false, true)),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    Err(first_error.unwrap_or(TrustError::NoTrustAnchor))
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
    /// A certificate's subject/SAN name fell outside the permitted subtrees, or
    /// inside an excluded subtree, imposed by a CA above it in the chain
    /// (RFC 5280 §4.2.1.10). `at` is the offending cert's chain index
    /// (0 = leaf).
    NameConstraintViolation {
        at: usize,
        kind: NcViolation,
    },
    Parse(CertParseError),
    /// An OCSP responder returned a definite `revoked` `CertStatus` for the
    /// certificate at chain index `at` (0 = leaf). Never suppressed by
    /// `SOFT_FAIL` — soft-fail only covers failure to *obtain* an answer.
    Revoked {
        at: usize,
    },
    /// Revocation checking was configured (`RevocationConfig` present) but no
    /// definite answer could be obtained for the certificate at chain index
    /// `at` — responder unreachable, timed out, returned a malformed/erroring
    /// response, or answered `unknown` — and `SOFT_FAIL` was not set. `reason`
    /// carries a human-readable diagnostic (network error text, parse
    /// failure, HTTP status, etc.).
    RevocationCheckFailed {
        at: usize,
        reason: String,
    },
}

/// The specific name that triggered a [`TrustError::NameConstraintViolation`].
#[derive(Debug, Clone)]
pub enum NcViolation {
    Dns(String),
    Ip(Vec<u8>),
    Email(String),
    Uri(String),
    DirName,
}

impl std::fmt::Display for NcViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NcViolation::Dns(s) => write!(f, "dNSName {:?}", s),
            NcViolation::Ip(b) => write!(f, "iPAddress {:02x?}", b),
            NcViolation::Email(s) => write!(f, "rfc822Name {:?}", s),
            NcViolation::Uri(s) => write!(f, "URI {:?}", s),
            NcViolation::DirName => f.write_str("directoryName"),
        }
    }
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
            TrustError::NameConstraintViolation { at, kind } => {
                write!(
                    f,
                    "name-constraint violation at index {}: {} not within permitted/within excluded subtrees",
                    at, kind
                )
            }
            TrustError::Parse(e) => write!(f, "parse: {}", e),
            TrustError::Revoked { at } => {
                write!(f, "certificate at index {} has been revoked (OCSP)", at)
            }
            TrustError::RevocationCheckFailed { at, reason } => {
                write!(
                    f,
                    "revocation check failed at index {} and SOFT_FAIL is not set: {}",
                    at, reason
                )
            }
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
///   5. Find the configured trust anchor: the last cert may be the stored
///      anchor itself, or it may be signed by an anchor whose subject matches
///      the last cert's issuer.
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
    // The presented order is a valid path far more often than not, so try it
    // first and keep its verdict.
    let presented = validate_ordered_chain(chain, trust);
    if presented.is_ok() {
        return presented;
    }
    // PKIX path BUILDING (RFC 5280 §6). What a caller hands
    // `checkClientTrusted`/`checkServerTrusted` is a certificate SET; the
    // ordered path has to be built from it, and this function previously
    // required the caller to have built it already. See `select_path` for the
    // two legal shapes that were refused — a cross-signed intermediate, and a
    // chain sent out of order.
    //
    // Deliberately structured so that building can only turn a REJECTION into
    // an ACCEPTANCE: the rebuilt path is put through the very same validation,
    // and if that does not fully succeed the ORIGINAL error is returned
    // unchanged. No error variant, index or message moves because of this
    // block, which is what keeps the existing rejection tests meaningful.
    if let Some(rebuilt) = rebuild_path(chain, trust) {
        if validate_ordered_chain(&rebuilt, trust).is_ok() {
            return Ok(());
        }
    }
    presented
}

/// Re-order `chain` into a path from the end entity, or `None` when the
/// presented order is already that path (so the caller has nothing to retry).
///
/// Parsing here repeats what `validate_ordered_chain` just did, which is
/// deliberate: this runs only on the failure path, where one extra parse of a
/// handful of certificates is not worth threading parsed state through the
/// success path for.
fn rebuild_path(chain: &[Vec<u8>], trust: &TrustManagerState) -> Option<Vec<Vec<u8>>> {
    if chain.len() < 2 {
        return None;
    }
    let mut parsed: Vec<ParsedCert> = Vec::with_capacity(chain.len());
    for der in chain {
        parsed.push(parse_certificate(der).ok()?);
    }
    let path = select_path(&parsed, trust);
    if path.len() == chain.len() && path.iter().enumerate().all(|(i, &j)| i == j) {
        return None;
    }
    Some(path.iter().map(|&i| chain[i].clone()).collect())
}

fn validate_ordered_chain(
    chain: &[Vec<u8>],
    trust: &TrustManagerState,
) -> Result<(), TrustError> {
    if chain.is_empty() {
        return Err(TrustError::EmptyChain);
    }

    // Step 1: parse.
    let mut parsed: Vec<ParsedCert> = Vec::with_capacity(chain.len());
    for der in chain {
        parsed.push(parse_certificate(der).map_err(TrustError::Parse)?);
    }

    // `duration_since(UNIX_EPOCH)` is an `Err` exactly when the clock reads
    // BEFORE 1970, and its error carries how far before. The previous
    // `.unwrap_or(0)` threw that away and clamped to 1970-01-01, which is not a
    // neutral default: every certificate in every chain has a `notBefore` after
    // it, so a pre-epoch clock rejected every chain `NotYetValid` while
    // reporting a timestamp the machine never had. Negate the error's duration
    // instead — `now` is then the real signed seconds-since-epoch, the
    // comparisons below stay truthful, and a machine whose clock says 1969
    // gets `NotYetValid` because it IS before the certificate's validity, not
    // because the value was swallowed.
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Cast: `as i64` on a pre-epoch offset that would overflow means a
        // clock more than 292 billion years off; saturate rather than wrap.
        Err(e) => -(e.duration().as_secs().min(i64::MAX as u64) as i64),
    };

    // Step 2: clock check — for the certificates on the PATH.
    //
    // A presented certificate that is itself a stored trust anchor is not on
    // the path: RFC 5280 §6.1 takes the anchor as an *input* to path
    // validation and validates `certificate 1..n` against it, so the anchor's
    // own validity period is never one of the things checked. JSSE agrees by
    // construction — `PKIXValidator` strips a trailing trusted certificate off
    // the chain before handing the remainder to `CertPathValidator`, so a
    // one-element chain that IS an anchor validates as the EMPTY path.
    //
    // netty's `testMutualAuthDiffCerts` is exactly that shape and is why this
    // exists: `test2.crt` is a self-signed certificate that expired in
    // November 2014 and is installed as the server's only trust anchor, and
    // the client presents it as its own identity. HotSpot completes that
    // handshake; this VM answered `checkClientTrusted` with
    // `CertificateException` and the client saw `certificate_unknown`.
    //
    // Scope is deliberately narrow — the certificate must be one the
    // application PUT in the trust store, compared by full DER. An expired
    // leaf that merely shares a subject with an anchor is still expired.
    for (i, p) in parsed.iter().enumerate() {
        if cert_is_stored_anchor(trust, &chain[i], p) {
            continue;
        }
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
            if parsed[i].basic_constraints_ca == Some(false) {
                return Err(TrustError::NotCa { at: i });
            }
            // A certificate that ISSUED another one must SAY it is a CA: RFC
            // 5280 §6.1.4(k) makes `basicConstraints` with `cA=TRUE` mandatory
            // for that position, and the JDK enforces it — `PKIXValidator`
            // answers "basic constraints check failed: this is not a CA
            // certificate", `SunX509` answers "End user tried to act as a CA".
            //
            // The `is_v3` guard this replaces exempted a v1 certificate from
            // the check entirely, on the reasoning that a legacy v1 ROOT has no
            // extensions to read. That is true of a root, and only of a root: a
            // self-signed anchor still gets the exemption below. Applied to an
            // INTERMEDIATE it inverted the rule — a v1 certificate carries no
            // `basicConstraints`, which is exactly why it may not be a CA, and
            // this accepted it as one. netty's `mutual_auth_invalid_client.p12`
            // is built around that: its v1 intermediate is what makes the
            // fixture "invalid", and both
            // `testMutualAuthInvalidIntermediateCAFailWith{Optional,Required}
            // ClientAuth` assert the handshake is REFUSED. They were passing
            // only because an unrelated error refused it first.
            let is_self_issued = parsed[i].issuer_der == parsed[i].subject_der;
            if !is_self_issued && parsed[i].basic_constraints_ca.is_none() {
                return Err(TrustError::NotCa { at: i });
            }
        }
    }

    // Step 5: trust anchor.
    // The last cert's issuer must be present in the trust set unless the last
    // cert is the actual stored trust anchor. Subject-DN equality is only an
    // index lookup: same-subject peer certificates are verified against every
    // candidate anchor SPKI, and only the concrete stored anchor that matches
    // or verifies is used for the rest of validation.
    let (anchor, last_is_anchor, last_signature_verified) =
        select_trust_anchor(&parsed, chain, trust)?;

    // Step 5b: name constraints (RFC 5280 §4.2.1.10). Enforce every CA's
    // permitted/excluded subtrees against the names of each certificate it
    // (transitively) issued. Independent of the signature step below — name
    // constraints bind on names, not keys.
    check_name_constraints(&parsed, anchor, last_is_anchor)?;

    // Step 6: cryptographic signature verification.
    //
    // For each cert[i] in the chain we re-verify its `signatureValue` against
    // the issuer's `SubjectPublicKeyInfo`. Non-final certificates use the next
    // chain cert's SPKI. The final cert is either the concrete stored anchor
    // and skipped by RFC 5280 section 6.1.1, or was already verified against
    // the matched anchor SPKI while resolving same-subject anchor candidates.
    //
    // The OID dispatch covers the two algorithms real-world JARs and PKIX
    // chains overwhelmingly use today; everything else maps to
    // `NotImplemented { oid }` so the caller can choose to defer to JCE.
    for i in 0..parsed.len() {
        if parsed[i].signature_value.is_empty() {
            return Err(TrustError::SignatureFailed { at: i });
        }
        if i + 1 == parsed.len() && (last_is_anchor || last_signature_verified) {
            continue;
        }
        let issuer_spki: &[u8] = if i + 1 < parsed.len() {
            parsed[i + 1].spki_der.as_slice()
        } else {
            anchor.spki_der.as_slice()
        };
        verify_one_signature(i, &parsed[i], issuer_spki)?;
    }

    // Step 7: OCSP revocation checking. Only runs when the active trust
    // manager carries a `RevocationConfig` (i.e. the caller attached a
    // `PKIXRevocationChecker` via `PKIXBuilderParameters.addCertPathChecker`
    // — see `extract_revocation_checker`). Absent that, this step is a no-op:
    // a structurally- and cryptographically-valid chain from a trusted CA is
    // accepted without a revocation opinion, matching the historical
    // behaviour of every trust manager that never asked for revocation
    // checking in the first place.
    if let Some(revocation) = &trust.revocation {
        check_revocation(&parsed, anchor, last_is_anchor, revocation)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// RFC 5280 §4.2.1.10 name constraints
// ---------------------------------------------------------------------------
//
// The RFC §6.1.4(g) state machine maintains, as it walks the path from the
// trust anchor down to the leaf, a `permitted_subtrees` set (intersected at
// each CA) and an `excluded_subtrees` set (unioned at each CA). The membership
// test we actually need — "is name N within the permitted_subtrees" — is
// equivalent to "N is within *every* contributing CA's permittedSubtrees", and
// "is N within the excluded_subtrees" is equivalent to "N is within *some*
// CA's excludedSubtrees". So instead of materialising the intersection/union
// we check each subordinate certificate's names directly against every CA above
// it. Chains are short (≤ a handful of certs), so the O(n²) walk is trivial.
//
// Coverage: dNSName, rfc822Name (email), uniformResourceIdentifier (host),
// iPAddress (CIDR), and directoryName (RDN prefix). GeneralName types this
// verifier does not model are neither parsed into subtrees nor checked.

/// Enforce name constraints over the whole parsed chain. `anchor` is the matched
/// trust anchor; `last_is_anchor` is true when that anchor is also the last cert
/// in `parsed` (a self-signed root shipped in the chain).
fn check_name_constraints(
    parsed: &[ParsedCert],
    anchor: &AnchorInfo,
    last_is_anchor: bool,
) -> Result<(), TrustError> {
    let n = parsed.len();

    // A name-constrained root that is NOT shipped in the chain still binds
    // every certificate beneath it; parse it from the stored anchor DER. When
    // the anchor IS the last cert in `parsed`, its constraints are already
    // reachable through the loop below, so we skip the extra parse.
    let ext_anchor_nc: Option<NameConstraints> = if last_is_anchor {
        None
    } else {
        anchor
            .full_cert_der
            .as_ref()
            .and_then(|der| parse_certificate(der).ok())
            .and_then(|a| a.name_constraints)
    };

    // Certificates subject to name-constraint checking: everything except the
    // trust anchor itself (a trust anchor is authoritative — RFC 5280 §6.1.1).
    // When the last cert is the anchor, exclude it; otherwise check all.
    let top_checked = if last_is_anchor {
        n.saturating_sub(1)
    } else {
        n
    };

    for j in 0..top_checked {
        let cert = &parsed[j];

        // RFC 5280 §6.1.3(b): a self-issued certificate that is not the final
        // (leaf) certificate is not checked against name constraints.
        let is_self_issued = cert.issuer_der == cert.subject_der;
        if is_self_issued && j != 0 {
            continue;
        }

        // Every CA above `cert` in the chain constrains it.
        for k in (j + 1)..n {
            if let Some(nc) = &parsed[k].name_constraints {
                enforce_name_constraints(cert, nc)
                    .map_err(|kind| TrustError::NameConstraintViolation { at: j, kind })?;
            }
        }
        if let Some(nc) = &ext_anchor_nc {
            enforce_name_constraints(cert, nc)
                .map_err(|kind| TrustError::NameConstraintViolation { at: j, kind })?;
        }
    }
    Ok(())
}

/// Check one subordinate certificate's names against one CA's `NameConstraints`.
/// Returns `Err(NcViolation)` for the first name that is inside an excluded
/// subtree, or — when that name type is restricted by a permitted subtree —
/// outside every permitted subtree. Names of a type the CA does not restrict
/// are unaffected (RFC 5280 §4.2.1.10).
fn enforce_name_constraints(cert: &ParsedCert, nc: &NameConstraints) -> Result<(), NcViolation> {
    // dNSName
    for name in &cert.san_dns_names {
        if nc
            .excluded
            .dns
            .iter()
            .any(|c| dns_constraint_matches(c, name))
        {
            return Err(NcViolation::Dns(name.clone()));
        }
        if !nc.permitted.dns.is_empty()
            && !nc
                .permitted
                .dns
                .iter()
                .any(|c| dns_constraint_matches(c, name))
        {
            return Err(NcViolation::Dns(name.clone()));
        }
    }

    // iPAddress
    for ip in &cert.san_ip_addresses {
        if nc.excluded.ip.iter().any(|c| ip_constraint_matches(c, ip)) {
            return Err(NcViolation::Ip(ip.clone()));
        }
        if !nc.permitted.ip.is_empty()
            && !nc.permitted.ip.iter().any(|c| ip_constraint_matches(c, ip))
        {
            return Err(NcViolation::Ip(ip.clone()));
        }
    }

    // rfc822Name (email)
    for email in &cert.san_rfc822_names {
        if nc
            .excluded
            .email
            .iter()
            .any(|c| email_constraint_matches(c, email))
        {
            return Err(NcViolation::Email(email.clone()));
        }
        if !nc.permitted.email.is_empty()
            && !nc
                .permitted
                .email
                .iter()
                .any(|c| email_constraint_matches(c, email))
        {
            return Err(NcViolation::Email(email.clone()));
        }
    }

    // uniformResourceIdentifier — the constraint applies to the URI's host.
    for uri in &cert.san_uris {
        match uri_host(uri) {
            Some(host) => {
                if nc
                    .excluded
                    .uri
                    .iter()
                    .any(|c| dns_constraint_matches(c, &host))
                {
                    return Err(NcViolation::Uri(uri.clone()));
                }
                if !nc.permitted.uri.is_empty()
                    && !nc
                        .permitted
                        .uri
                        .iter()
                        .any(|c| dns_constraint_matches(c, &host))
                {
                    return Err(NcViolation::Uri(uri.clone()));
                }
            }
            None => {
                // No extractable host but a permitted-URI constraint exists:
                // we cannot prove the URI is within the permitted set, so fail
                // closed rather than admit it.
                if !nc.permitted.uri.is_empty() {
                    return Err(NcViolation::Uri(uri.clone()));
                }
            }
        }
    }

    // directoryName — applies to the subject DN and any SAN directoryName.
    // Empty DNs (zero RDNs) carry no directoryName to constrain and are skipped.
    let mut dir_names: Vec<&[u8]> = Vec::new();
    if rdn_count(&cert.subject_der) > 0 {
        dir_names.push(cert.subject_der.as_slice());
    }
    for d in &cert.san_dir_names {
        if rdn_count(d) > 0 {
            dir_names.push(d.as_slice());
        }
    }
    for dn in dir_names {
        if nc.excluded.dir.iter().any(|c| dir_name_within(c, dn)) {
            return Err(NcViolation::DirName);
        }
        if !nc.permitted.dir.is_empty() && !nc.permitted.dir.iter().any(|c| dir_name_within(c, dn))
        {
            return Err(NcViolation::DirName);
        }
    }

    Ok(())
}

/// RFC 5280 §4.2.1.10 dNSName matching. A constraint matches a presented name
/// if the name can be formed by prepending zero or more labels to the
/// constraint (`example.com` matches `example.com` and `www.example.com`, but
/// not `notexample.com`). An empty constraint matches everything. A constraint
/// with a leading `.` (`.example.com`) is honoured as subdomain-only: it
/// matches strict subdomains but not the bare domain. Both arguments must be
/// lower-cased by the caller.
fn dns_constraint_matches(constraint: &str, presented: &str) -> bool {
    if constraint.is_empty() {
        return true;
    }
    if let Some(_bare) = constraint.strip_prefix('.') {
        // ".example.com" — match strict subdomains only.
        return presented.len() > constraint.len() && presented.ends_with(constraint);
    }
    if presented == constraint {
        return true;
    }
    // Suffix match with a label boundary: presented == "<labels>." + constraint.
    presented.len() > constraint.len()
        && presented.ends_with(constraint)
        && presented.as_bytes()[presented.len() - constraint.len() - 1] == b'.'
}

/// RFC 5280 §4.2.1.10 rfc822Name (email) matching. The constraint is either a
/// full mailbox (`user@host` — exact match), a host (`host` — matches every
/// mailbox at that host), or a domain with a leading `.` (`.example.com` —
/// matches every mailbox whose host is a subdomain). All lower-cased.
fn email_constraint_matches(constraint: &str, presented: &str) -> bool {
    if constraint.is_empty() {
        return true;
    }
    if constraint.contains('@') {
        return presented == constraint;
    }
    // Constraint restricts the host part of the presented mailbox.
    let host = match presented.rsplit_once('@') {
        Some((_, h)) if !h.is_empty() => h,
        _ => return false,
    };
    if let Some(_bare) = constraint.strip_prefix('.') {
        return host.len() > constraint.len() && host.ends_with(constraint);
    }
    host == constraint
}

/// RFC 5280 §4.2.1.10 iPAddress matching. `constraint` is `address || mask`
/// (8 bytes IPv4, 32 bytes IPv6); `presented` is a raw address (4 / 16 bytes).
/// Families must match and `presented & mask == address & mask`.
fn ip_constraint_matches(constraint: &[u8], presented: &[u8]) -> bool {
    let alen = match constraint.len() {
        8 => 4,
        32 => 16,
        _ => return false,
    };
    if presented.len() != alen {
        return false;
    }
    let (addr, mask) = constraint.split_at(alen);
    for i in 0..alen {
        if (presented[i] & mask[i]) != (addr[i] & mask[i]) {
            return false;
        }
    }
    true
}

/// Extract the lower-cased host from a URI for name-constraint matching:
/// `scheme://[userinfo@]host[:port][/path]`. Handles bracketed IPv6 literals.
/// Returns `None` when no authority/host can be isolated.
fn uri_host(uri: &str) -> Option<String> {
    let after = uri.split_once("://").map(|(_, b)| b).unwrap_or(uri);
    let authority = after.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, b)| b)
        .unwrap_or(authority);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6 literal: take up to ']'.
        rest.split_once(']').map(|(h, _)| h).unwrap_or(rest)
    } else if let Some((h, p)) = hostport.rsplit_once(':') {
        // Strip a trailing :port only when it is all digits; otherwise the
        // colon belongs to the host (defensive — unbracketed v6 won't appear
        // in a well-formed URI authority).
        if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            hostport
        }
    } else {
        hostport
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Count the RDNs in a `Name` SEQUENCE DER (an `RDNSequence`). Returns 0 on a
/// structural error or an empty DN.
fn rdn_count(name_der: &[u8]) -> usize {
    split_rdns(name_der).map(|v| v.len()).unwrap_or(0)
}

/// Split a `Name` SEQUENCE DER into its RDN element DERs (each an outer SET),
/// in document (most-significant-first) order.
fn split_rdns(name_der: &[u8]) -> Option<Vec<Vec<u8>>> {
    let seq = read_tlv_tagged(name_der, TAG_SEQUENCE).ok()?;
    let mut out = Vec::new();
    let mut c = seq.content;
    while !c.is_empty() {
        let rdn = read_tlv(c).ok()?;
        out.push(rdn.full.to_vec());
        c = rdn.rest;
    }
    Some(out)
}

/// RFC 5280 §4.2.1.10 directoryName matching: the constraint DN matches the
/// presented DN when its RDN sequence is an initial prefix of the presented
/// DN's RDN sequence. Comparison is byte-exact per RDN (no attribute-value
/// string normalisation), which is correct for the canonical DER that issued
/// certificates use; the documented residual limit is that two RDNs that are
/// equal only after case-folding/whitespace-normalisation are treated as
/// distinct.
fn dir_name_within(constraint: &[u8], presented: &[u8]) -> bool {
    let cr = match split_rdns(constraint) {
        Some(v) => v,
        None => return false,
    };
    let pr = match split_rdns(presented) {
        Some(v) => v,
        None => return false,
    };
    if cr.len() > pr.len() {
        return false;
    }
    cr.iter().zip(pr.iter()).all(|(a, b)| a == b)
}

/// `id-mgf1` — the only mask generation function RFC 4055 defines.
const OID_MGF1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08];

/// The three things `RSASSA-PSS-params` says that the signature OID does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsaPssParams {
    pub hash: crate::crypto_impl::PssHash,
    pub mgf_hash: crate::crypto_impl::PssHash,
    pub salt_len: usize,
}

/// Parse the `parameters` of an RSASSA-PSS `AlgorithmIdentifier` (RFC 4055 §3.1):
///
/// ```text
/// RSASSA-PSS-params ::= SEQUENCE {
///     hashAlgorithm    [0] HashAlgorithm    DEFAULT sha1,
///     maskGenAlgorithm [1] MaskGenAlgorithm DEFAULT mgf1SHA1,
///     saltLength       [2] INTEGER          DEFAULT 20,
///     trailerField     [3] TrailerField     DEFAULT trailerFieldBC }
/// ```
///
/// All four fields are OPTIONAL with defaults, and the defaults are **not**
/// self-consistent with each other the way the JWA PS256/384/512 convention
/// is: `saltLength` defaults to 20 whether the digest is SHA-1 or SHA-512.
/// Absent parameters (or an explicit NULL) therefore mean SHA-1/MGF1-SHA-1/20,
/// not "same as the OID".
///
/// `None` when the DER is malformed, when a digest OID is one we do not
/// implement, when the MGF is not MGF1, or when `trailerField` is anything but
/// the default 1 — every one of those is a signature this verifier must not
/// claim to have checked.
pub fn parse_rsa_pss_params(params_der: &[u8]) -> Option<RsaPssParams> {
    use crate::crypto_impl::PssHash;

    let mut out = RsaPssParams {
        hash: PssHash::Sha1,
        mgf_hash: PssHash::Sha1,
        salt_len: 20,
    };
    // Absent parameters, or ASN.1 NULL: every default applies.
    if params_der.is_empty() {
        return Some(out);
    }
    let outer = read_tlv(params_der).ok()?;
    if outer.tag == TAG_NULL {
        return Some(out);
    }
    if outer.tag != TAG_SEQUENCE {
        return None;
    }

    // Each field is EXPLICIT, i.e. a constructed context tag wrapping the
    // real value.
    let mut cursor = outer.content;
    while !cursor.is_empty() {
        let field = read_tlv(cursor).ok()?;
        cursor = field.rest;
        match field.tag {
            // [0] hashAlgorithm — AlgorithmIdentifier of the digest.
            0xa0 => {
                let alg = read_tlv_tagged(field.content, TAG_SEQUENCE).ok()?;
                let alg_oid = read_tlv_tagged(alg.content, TAG_OID).ok()?;
                out.hash = PssHash::from_digest_oid(alg_oid.content)?;
            }
            // [1] maskGenAlgorithm — AlgorithmIdentifier { id-mgf1, digest }.
            0xa1 => {
                let alg = read_tlv_tagged(field.content, TAG_SEQUENCE).ok()?;
                let alg_oid = read_tlv_tagged(alg.content, TAG_OID).ok()?;
                if alg_oid.content != OID_MGF1 {
                    return None;
                }
                let inner = read_tlv_tagged(alg_oid.rest, TAG_SEQUENCE).ok()?;
                let inner_oid = read_tlv_tagged(inner.content, TAG_OID).ok()?;
                out.mgf_hash = PssHash::from_digest_oid(inner_oid.content)?;
            }
            // [2] saltLength — INTEGER.
            0xa2 => {
                let int = read_tlv_tagged(field.content, TAG_INTEGER).ok()?;
                // Non-negative and small: a salt longer than a few hundred
                // bytes cannot fit any modulus we support, and a negative one
                // is malformed.
                if int.content.is_empty() || int.content.len() > 4 || int.content[0] & 0x80 != 0 {
                    return None;
                }
                let mut v: usize = 0;
                for b in int.content {
                    v = (v << 8) | (*b as usize);
                }
                out.salt_len = v;
            }
            // [3] trailerField — INTEGER, only 1 (0xbc) is defined.
            0xa3 => {
                let int = read_tlv_tagged(field.content, TAG_INTEGER).ok()?;
                if int.content != [0x01] {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(out)
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
    use crate::crypto_impl::{
        parse_named_ec_public_key, parse_rsa_public_key, verify_named_ecdsa, Rsa, Sha256, Sha384,
        Sha512,
    };
    use cratonvm_native_builtins_crypto::signature::DigestAlgorithm;

    let oid = cert.signature_algorithm_oid.as_slice();
    let sig = cert.signature_value.as_slice();
    let tbs = cert.tbs_bytes.as_slice();

    // PKCS#1 v1.5 RSA, digest chosen by OID. The verification core
    // (`crypto::signature::verify_rsa_pkcs1_v15_checked`) is already
    // digest-parameterised, so this is a dispatch table rather than a copy per
    // algorithm — the in-tree `Rsa::pkcs1v15_encode`, whose DigestInfo prefix
    // IS hard-coded to SHA-256, is not on this path.
    let rsa_digest = if oid == OID_SIG_SHA256_RSA {
        Some(DigestAlgorithm::Sha256)
    } else if oid == OID_SIG_SHA1_RSA {
        Some(DigestAlgorithm::Sha1)
    } else if oid == OID_SIG_SHA384_RSA {
        Some(DigestAlgorithm::Sha384)
    } else if oid == OID_SIG_SHA512_RSA {
        Some(DigestAlgorithm::Sha512)
    } else {
        None
    };

    if let Some(digest_alg) = rsa_digest {
        let pk = match parse_rsa_public_key(issuer_spki) {
            Some(k) => k,
            None => return Err(TrustError::BadSignature { at }),
        };
        if Rsa::verify_pkcs1_v15(&pk, digest_alg, tbs, sig) {
            Ok(())
        } else {
            Err(TrustError::BadSignature { at })
        }
    } else if oid == OID_SIG_ECDSA_SHA256
        || oid == OID_SIG_ECDSA_SHA384
        || oid == OID_SIG_ECDSA_SHA512
    {
        // ECDSA: DER-decoded (r, s), check u1*G + u2*Q.x ≡ r (mod n).
        // `verify_named_ecdsa` takes a PRE-HASHED digest and truncates it to
        // the curve order's bit length itself (FIPS 186-4 §6.4), which is
        // exactly why SHA-384/512 need no separate verify path — only the
        // right hash over the TBS.
        //
        // ON THE CURVE, not on P-256. `parse_ecdsa_public_key` (still used by
        // the JCE-facing P-256 paths) discards the SPKI's named-curve OID and
        // then requires a 65-byte point, so a P-384 issuer key came back as
        // `None` and this returned `BadSignature` — a refusal indistinguishable
        // from a forged certificate. MEASURED across 20 live public sites the
        // first time this validator was handed real chains: SIX rejected, all
        // six with a P-384 issuer (Let's Encrypt Root YE / YE1 / YE2, Sectigo
        // Root E46, DigiCert Global G3 TLS ECC, Google GTS Root R4). See
        // `crypto_impl`'s named-curve section for why that was invisible until
        // the leaf-only chain capture was fixed.
        let pk = match parse_named_ec_public_key(issuer_spki) {
            Some(k) => k,
            None => return Err(TrustError::BadSignature { at }),
        };
        let digest: Vec<u8> = if oid == OID_SIG_ECDSA_SHA384 {
            Sha384::digest(tbs).to_vec()
        } else if oid == OID_SIG_ECDSA_SHA512 {
            Sha512::digest(tbs).to_vec()
        } else {
            Sha256::digest(tbs).to_vec()
        };
        if verify_named_ecdsa(&pk, &digest, sig) {
            Ok(())
        } else {
            Err(TrustError::BadSignature { at })
        }
    } else if oid == OID_SIG_RSA_PSS {
        // RSASSA-PSS (1.2.840.113549.1.1.10). Unlike every other signature
        // OID in this table, the OID alone does NOT say which digest was used,
        // what digest MGF1 runs over, or how long the salt is — those live in
        // the `RSASSA-PSS-params` AlgorithmIdentifier parameters, so they have
        // to be parsed (`parse_rsa_pss_params`).
        //
        // Guessing them does not work, which is what the previous
        // trial-three-digests-at-salt-length-hLen version measured: netty's
        // `rsapss-ca-cert.cert` and the two `rsaValidation*.p12` fixtures are
        // SHA-256 / MGF1-SHA-256 with a **20-byte** salt — RFC 4055 §3.1's
        // DEFAULT saltLength, which is 20 whatever the digest is, not hLen.
        // Every trial therefore failed and the chain came back
        // `BadSignature`, i.e. a REJECTED chain and a `certificate_unknown`
        // alert (`testRSASSAPSS`).
        let params = match parse_rsa_pss_params(&cert.signature_algorithm_params) {
            Some(p) => p,
            // Unparseable or naming a digest we do not implement: structural,
            // not cryptographic, so callers can still choose to delegate.
            None => {
                return Err(TrustError::NotImplemented {
                    at,
                    oid: oid.to_vec(),
                })
            }
        };
        let pk = match parse_rsa_public_key(issuer_spki) {
            Some(k) => k,
            None => return Err(TrustError::BadSignature { at }),
        };
        let n = pk.n.to_bytes_be();
        let e = pk.e.to_bytes_be();
        if crate::crypto_impl::rsa_verify_pss_ex(
            &n,
            &e,
            params.hash,
            params.mgf_hash,
            params.salt_len,
            tbs,
            sig,
        ) {
            Ok(())
        } else {
            Err(TrustError::BadSignature { at })
        }
    } else if oid == OID_SIG_DSA_SHA1
        || oid == OID_SIG_ED25519
        || oid == OID_SIG_SHA224_RSA
        || oid == OID_SIG_ECDSA_SHA224
    {
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
// OCSP revocation checking (RFC 6960)
// ---------------------------------------------------------------------------
//
// Real OCSP: a DER `OCSPRequest` is POSTed to a responder over plain HTTP
// (RFC 6960 Appendix A.1 — OCSP has its own MIME types and does not require
// TLS; responders are conventionally plain HTTP, and that is what the test
// harness's `TesterOcspResponderServlet` exposes), the DER `OCSPResponse` is
// parsed, and the embedded `BasicOCSPResponse`'s signature is verified
// against either the issuing CA's key directly, or a delegated responder
// certificate (carried in the response's own `certs` field, itself signed by
// the issuing CA, or matching an explicitly configured
// `PKIXRevocationChecker.setOcspResponderCert(...)`).
//
// `CertID` uses SHA-1 (RFC 6960's own conventional default — deliberately
// weak-hash-tolerant because it hashes only public, non-secret identifiers:
// issuer name and issuer public key, not anything an attacker could forge a
// preimage for that would matter cryptographically here) unless a future
// caller negotiates something else; this verifier only ever emits SHA-1
// `CertID`s, matching what `TesterOcspResponderServlet`
// (`RespID(..., digestCalculatorProvider.get(SHA-1))`, "Only SHA-1
// supported") and every other OCSP responder in practice expects.

/// OID for `id-pkix-ocsp-basic` (1.3.6.1.5.5.7.48.1.1) — the
/// `ResponseBytes.responseType` value whose `response` OCTET STRING content
/// is a DER `BasicOCSPResponse`. The only response type this verifier (or
/// any responder we've seen) produces.
const OID_OCSP_BASIC_RESPONSE: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01, 0x01];

/// SHA-1 `AlgorithmIdentifier` OID (1.3.14.3.2.26) used for `CertID`'s
/// `hashAlgorithm` — RFC 6960's conventional default.
const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];

/// Outcome of checking one certificate's revocation status against an OCSP
/// responder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OcspOutcome {
    /// `CertStatus ::= good [0] IMPLICIT NULL` — definitely not revoked.
    Good,
    /// `CertStatus ::= revoked [1] IMPLICIT RevokedInfo` — definitely revoked.
    /// Never soft-failed, even when `SOFT_FAIL` is configured.
    Revoked,
    /// The responder could not be reached, timed out, returned a transport
    /// error, returned a malformed response, returned a non-`successful`
    /// `OCSPResponseStatus` (`tryLater`, `internalError`, ...), or returned
    /// `unknown [2]` for this specific cert. Soft-failable when `SOFT_FAIL`
    /// is configured; otherwise treated as a hard validation failure.
    Indeterminate(String),
}

/// A minimal parsed `CertID` — the fields needed to build an `OCSPRequest`
/// for one certificate. `issuer_name_hash`/`issuer_key_hash` are SHA-1(DER
/// issuer Name) / SHA-1(issuer SPKI's `subjectPublicKey` BIT STRING content,
/// i.e. the raw key bytes without the unused-bits-count byte) per RFC 6960
/// §4.1.1.
struct CertId {
    issuer_name_hash: [u8; 20],
    issuer_key_hash: [u8; 20],
    serial_der: Vec<u8>,
}

fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(data);
    h.finalize().into()
}

/// Build the `CertID` for `subject` (a chain certificate), whose issuer is
/// `issuer_spki_der`/`issuer_subject_der` (either the next cert up the chain,
/// or the matched trust anchor's own cert when `subject` is signed directly
/// by the anchor).
fn build_cert_id(
    subject: &ParsedCert,
    issuer_subject_der: &[u8],
    issuer_spki_der: &[u8],
) -> Option<CertId> {
    // issuer_key_hash = SHA-1 over the raw key bits (the SubjectPublicKeyInfo
    // BIT STRING's content, minus its leading unused-bits-count byte) — NOT
    // over the whole SPKI SEQUENCE. RFC 6960 §4.1.1 defines this as
    // SHA-1(the value of the BIT STRING subjectPublicKey, excluding tag,
    // length, and unused-bits).
    let spki = read_tlv_tagged(issuer_spki_der, TAG_SEQUENCE).ok()?;
    let alg = read_tlv_tagged(spki.content, TAG_SEQUENCE).ok()?;
    let bs = read_tlv_tagged(alg.rest, TAG_BIT_STRING).ok()?;
    if bs.content.is_empty() {
        return None;
    }
    let key_bits = &bs.content[1..];
    Some(CertId {
        issuer_name_hash: sha1(issuer_subject_der),
        issuer_key_hash: sha1(key_bits),
        serial_der: subject.serial_der.clone(),
    })
}

// ---- Minimal DER encoder (production path — the `#[cfg(test)]` fixture
// builders below in `mod tests` are not visible outside that module) ----

fn der_encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let first_nonzero = bytes
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(bytes.len() - 1);
        let sig = &bytes[first_nonzero..];
        out.push(0x80 | sig.len() as u8);
        out.extend_from_slice(sig);
    }
}

fn der_tlv_encode(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    der_encode_length(body.len(), &mut out);
    out.extend_from_slice(body);
    out
}

fn der_seq_encode(body: Vec<u8>) -> Vec<u8> {
    der_tlv_encode(TAG_SEQUENCE, &body)
}

fn der_oid_encode(body: &[u8]) -> Vec<u8> {
    der_tlv_encode(TAG_OID, body)
}

fn der_octet_encode(body: &[u8]) -> Vec<u8> {
    der_tlv_encode(TAG_OCTET_STRING, body)
}

fn der_null_encode() -> Vec<u8> {
    der_tlv_encode(TAG_NULL, &[])
}

/// Minimal-length two's-complement `INTEGER` encoding for a serial that is
/// already stored as raw (already-minimal, non-negative per RFC 5280) DER
/// integer content bytes — re-wrap with tag/length only, no re-minimisation
/// (the source is itself DER, already minimal).
fn der_integer_from_content(content: &[u8]) -> Vec<u8> {
    let body: &[u8] = if content.is_empty() { &[0u8] } else { content };
    der_tlv_encode(TAG_INTEGER, body)
}

/// Build a DER `OCSPRequest` (RFC 6960 §4.1.1) for a single `CertID`:
///
/// ```text
/// OCSPRequest     ::= SEQUENCE { tbsRequest TBSRequest }
/// TBSRequest      ::= SEQUENCE { requestList SEQUENCE OF Request }
/// Request         ::= SEQUENCE { reqCert CertID }
/// CertID          ::= SEQUENCE {
///     hashAlgorithm   AlgorithmIdentifier,
///     issuerNameHash  OCTET STRING,
///     issuerKeyHash   OCTET STRING,
///     serialNumber    CertificateSerialNumber }
/// ```
///
/// No `requestorName`, no `requestExtensions` (in particular, no nonce — the
/// test responder does not echo one, and omitting it is valid per RFC 6960).
fn build_ocsp_request(id: &CertId) -> Vec<u8> {
    let hash_alg = der_seq_encode({
        let mut v = der_oid_encode(OID_SHA1);
        v.extend_from_slice(&der_null_encode());
        v
    });
    let cert_id = der_seq_encode({
        let mut v = hash_alg;
        v.extend_from_slice(&der_octet_encode(&id.issuer_name_hash));
        v.extend_from_slice(&der_octet_encode(&id.issuer_key_hash));
        v.extend_from_slice(&der_integer_from_content(&id.serial_der));
        v
    });
    let request = der_seq_encode(cert_id);
    let request_list = der_seq_encode(request);
    let tbs_request = der_seq_encode(request_list);
    der_seq_encode(tbs_request)
}

/// A single `SingleResponse` extracted from a parsed `BasicOCSPResponse`.
#[derive(Debug)]
struct SingleResponse {
    cert_id_serial: Vec<u8>,
    status: OcspStatusTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OcspStatusTag {
    Good,
    Revoked,
    Unknown,
}

/// A parsed `BasicOCSPResponse` (RFC 6960 §4.2.1), enough of it to verify the
/// signature and read every `SingleResponse`'s status.
#[derive(Debug)]
struct BasicOcspResponse {
    tbs_response_data_der: Vec<u8>,
    signature_algorithm_oid: Vec<u8>,
    signature: Vec<u8>,
    /// Optional `certs [0]` — delegated responder certificate chain, leaf
    /// (the actual signer) first, in document order.
    certs: Vec<Vec<u8>>,
    responses: Vec<SingleResponse>,
}

/// Parse a DER `OCSPResponse` all the way down to a `BasicOcspResponse`, or
/// `Err` with a human-readable reason (surfaced through
/// `OcspOutcome::Indeterminate` so callers can decide soft-fail).
///
/// ```text
/// OCSPResponse ::= SEQUENCE {
///    responseStatus   OCSPResponseStatus,
///    responseBytes    [0] EXPLICIT ResponseBytes OPTIONAL }
/// OCSPResponseStatus ::= ENUMERATED {
///    successful (0), malformedRequest (1), internalError (2),
///    tryLater (3), (4 unused), sigRequired (5), unauthorized (6) }
/// ResponseBytes ::= SEQUENCE {
///    responseType   OBJECT IDENTIFIER,
///    response       OCTET STRING }
/// ```
fn parse_ocsp_response(der: &[u8]) -> Result<BasicOcspResponse, String> {
    let outer = read_tlv_tagged(der, TAG_SEQUENCE).map_err(|e| format!("OCSPResponse: {e}"))?;
    let status = read_tlv_tagged(outer.content, TAG_ENUMERATED)
        .map_err(|e| format!("responseStatus: {e}"))?;
    let status_code = status.content.first().copied().unwrap_or(0xff);
    if status_code != 0 {
        let name = match status_code {
            1 => "malformedRequest",
            2 => "internalError",
            3 => "tryLater",
            5 => "sigRequired",
            6 => "unauthorized",
            _ => "unknown-status",
        };
        return Err(format!(
            "responder returned non-successful status: {name} ({status_code})"
        ));
    }
    let rb_outer = read_tlv(status.rest).map_err(|e| format!("responseBytes: {e}"))?;
    if rb_outer.tag != TAG_CONTEXT_0 {
        return Err("successful response with no responseBytes".to_string());
    }
    let rb = read_tlv_tagged(rb_outer.content, TAG_SEQUENCE)
        .map_err(|e| format!("ResponseBytes: {e}"))?;
    let response_type =
        read_tlv_tagged(rb.content, TAG_OID).map_err(|e| format!("responseType: {e}"))?;
    if response_type.content != OID_OCSP_BASIC_RESPONSE {
        return Err("unsupported OCSP responseType (not id-pkix-ocsp-basic)".to_string());
    }
    let response_octets = read_tlv_tagged(response_type.rest, TAG_OCTET_STRING)
        .map_err(|e| format!("response: {e}"))?;

    // BasicOCSPResponse ::= SEQUENCE { tbsResponseData, signatureAlgorithm,
    //   signature BIT STRING, certs [0] EXPLICIT SEQUENCE OF Certificate OPTIONAL }
    let basic = read_tlv_tagged(response_octets.content, TAG_SEQUENCE)
        .map_err(|e| format!("BasicOCSPResponse: {e}"))?;
    let tbs_response_data = read_tlv_tagged(basic.content, TAG_SEQUENCE)
        .map_err(|e| format!("tbsResponseData: {e}"))?;
    let tbs_response_data_der = tbs_response_data.full.to_vec();

    let sig_alg = read_tlv_tagged(tbs_response_data.rest, TAG_SEQUENCE)
        .map_err(|e| format!("signatureAlgorithm: {e}"))?;
    let sig_alg_oid =
        read_tlv_tagged(sig_alg.content, TAG_OID).map_err(|e| format!("sigAlg OID: {e}"))?;
    let signature_algorithm_oid = sig_alg_oid.content.to_vec();

    let sig_bs =
        read_tlv_tagged(sig_alg.rest, TAG_BIT_STRING).map_err(|e| format!("signature: {e}"))?;
    let signature = if sig_bs.content.is_empty() {
        Vec::new()
    } else {
        sig_bs.content[1..].to_vec()
    };

    // Optional certs [0] EXPLICIT SEQUENCE OF Certificate
    let mut certs = Vec::new();
    if !sig_bs.rest.is_empty() {
        if let Ok(certs_ctx) = read_tlv_tagged(sig_bs.rest, TAG_CONTEXT_0) {
            if let Ok(seq) = read_tlv_tagged(certs_ctx.content, TAG_SEQUENCE) {
                let mut cc = seq.content;
                while !cc.is_empty() {
                    let c = read_tlv(cc).map_err(|e| format!("certs[]: {e}"))?;
                    certs.push(c.full.to_vec());
                    cc = c.rest;
                }
            }
        }
    }

    // ---- Parse ResponseData ::= SEQUENCE { version [0] EXPLICIT INTEGER
    //   DEFAULT v1, responderID ResponderID, producedAt GeneralizedTime,
    //   responses SEQUENCE OF SingleResponse, responseExtensions [1]
    //   EXPLICIT Extensions OPTIONAL } ----
    let mut rd_cursor = tbs_response_data.content;
    let first = read_tlv(rd_cursor).map_err(|e| format!("ResponseData: {e}"))?;
    if first.tag == TAG_CONTEXT_0 {
        rd_cursor = first.rest;
    }
    // responderID CHOICE { byName [1] Name, byKey [2] OCTET STRING } —
    // context-tagged, either way; skip via generic TLV read.
    let responder_id = read_tlv(rd_cursor).map_err(|e| format!("responderID: {e}"))?;
    rd_cursor = responder_id.rest;
    // producedAt GeneralizedTime
    let produced_at =
        read_tlv_tagged(rd_cursor, TAG_GENERALIZED_TIME).map_err(|e| format!("producedAt: {e}"))?;
    rd_cursor = produced_at.rest;
    // responses SEQUENCE OF SingleResponse
    let responses_seq =
        read_tlv_tagged(rd_cursor, TAG_SEQUENCE).map_err(|e| format!("responses: {e}"))?;

    let mut responses = Vec::new();
    let mut sc = responses_seq.content;
    while !sc.is_empty() {
        // SingleResponse ::= SEQUENCE { certID CertID, certStatus CertStatus,
        //   thisUpdate GeneralizedTime, nextUpdate [0] EXPLICIT
        //   GeneralizedTime OPTIONAL, singleExtensions [1] EXPLICIT
        //   Extensions OPTIONAL }
        let sr = read_tlv_tagged(sc, TAG_SEQUENCE).map_err(|e| format!("SingleResponse: {e}"))?;
        sc = sr.rest;

        // CertID ::= SEQUENCE { hashAlgorithm, issuerNameHash OCTET STRING,
        //   issuerKeyHash OCTET STRING, serialNumber INTEGER }
        let cert_id =
            read_tlv_tagged(sr.content, TAG_SEQUENCE).map_err(|e| format!("CertID: {e}"))?;
        let hash_alg = read_tlv_tagged(cert_id.content, TAG_SEQUENCE)
            .map_err(|e| format!("CertID.hashAlgorithm: {e}"))?;
        let issuer_name_hash = read_tlv_tagged(hash_alg.rest, TAG_OCTET_STRING)
            .map_err(|e| format!("issuerNameHash: {e}"))?;
        let issuer_key_hash = read_tlv_tagged(issuer_name_hash.rest, TAG_OCTET_STRING)
            .map_err(|e| format!("issuerKeyHash: {e}"))?;
        let serial = read_tlv_tagged(issuer_key_hash.rest, TAG_INTEGER)
            .map_err(|e| format!("CertID.serialNumber: {e}"))?;
        let cert_id_serial = serial.content.to_vec();

        // certStatus ::= CHOICE { good [0] IMPLICIT NULL (primitive, 0x80),
        //   revoked [1] IMPLICIT RevokedInfo (constructed SEQUENCE, 0xa1),
        //   unknown [2] IMPLICIT UnknownInfo (primitive NULL, 0x82) }
        let cert_status = read_tlv(cert_id.rest).map_err(|e| format!("certStatus: {e}"))?;
        let status = match cert_status.tag {
            0x80 => OcspStatusTag::Good,
            TAG_CONTEXT_1_CONSTRUCTED => OcspStatusTag::Revoked,
            TAG_CONTEXT_2_PRIMITIVE => OcspStatusTag::Unknown,
            other => return Err(format!("unrecognised certStatus tag {other:#x}")),
        };

        responses.push(SingleResponse {
            cert_id_serial,
            status,
        });
    }

    Ok(BasicOcspResponse {
        tbs_response_data_der,
        signature_algorithm_oid,
        signature,
        certs,
        responses,
    })
}

/// Verify a `BasicOcspResponse`'s signature and return the trusted verdict
/// for `wanted_serial`'s `CertID` — or `Err` (soft-failable) when the
/// signature cannot be verified (bad crypto, no usable signer key found, or
/// this cert's serial has no `SingleResponse` at all).
///
/// Trust chain for the signer:
///   1. If the active `RevocationConfig` has an explicit
///      `responder_cert_der` (`PKIXRevocationChecker.setOcspResponderCert`),
///      the response MUST be signed by exactly that key — no further chain
///      check (matches real-JDK: an explicitly pinned responder cert is
///      trusted directly).
///   2. Otherwise, if the response carries a `certs[]` chain, its first
///      entry is the signer; that signer cert must (a) verify the response
///      signature, (b) carry the `id-kp-OCSPSigning` EKU, and (c) itself be
///      signed by `issuer_spki_der` (the CA that issued the certificate
///      being checked) — the standard "delegated responder" trust model
///      (RFC 6960 §4.2.2.2).
///   3. Otherwise, the issuing CA's own key must have produced the
///      signature directly.
fn verify_ocsp_response_signature(
    resp: &BasicOcspResponse,
    issuer_spki_der: &[u8],
    explicit_responder_cert_der: Option<&[u8]>,
) -> Result<(), String> {
    let verify_with_spki = |spki: &[u8]| -> bool {
        match resp.signature_algorithm_oid.as_slice() {
            oid if oid == OID_SIG_SHA256_RSA => {
                let Some(pk) = crate::crypto_impl::parse_rsa_public_key(spki) else {
                    return false;
                };
                crate::crypto_impl::Rsa::verify_sha256(
                    &pk,
                    &resp.tbs_response_data_der,
                    &resp.signature,
                )
            }
            oid if oid == OID_SIG_ECDSA_SHA256 => {
                // Named-curve, for the same reason the chain verifier is: an
                // OCSP responder under a P-384 CA is exactly as ordinary as a
                // certificate under one, and the P-256-only parser answered
                // `None` -- i.e. "signature invalid" -- for every such key.
                let Some(pk) = crate::crypto_impl::parse_named_ec_public_key(spki) else {
                    return false;
                };
                let digest = crate::crypto_impl::Sha256::digest(&resp.tbs_response_data_der);
                crate::crypto_impl::verify_named_ecdsa(&pk, &digest, &resp.signature)
            }
            _ => false,
        }
    };

    if let Some(pinned_der) = explicit_responder_cert_der {
        let pinned =
            parse_certificate(pinned_der).map_err(|e| format!("pinned responder cert: {e}"))?;
        return if verify_with_spki(&pinned.spki_der) {
            Ok(())
        } else {
            Err(
                "OCSP response signature does not verify against the pinned responder cert"
                    .to_string(),
            )
        };
    }

    if let Some(signer_der) = resp.certs.first() {
        let signer = parse_certificate(signer_der).map_err(|e| format!("responder cert: {e}"))?;
        if !verify_with_spki(&signer.spki_der) {
            return Err(
                "OCSP response signature does not verify against embedded responder cert"
                    .to_string(),
            );
        }
        if !signer
            .ext_key_usage
            .iter()
            .any(|eku| eku.as_slice() == OID_KP_OCSP_SIGNING)
        {
            return Err("embedded OCSP responder cert lacks id-kp-OCSPSigning EKU".to_string());
        }
        // The delegated responder cert must itself be signed by the same CA
        // that issued the certificate under check.
        if signer.signature_value.is_empty() {
            return Err("responder cert has no signature".to_string());
        }
        return match verify_one_signature(0, &signer, issuer_spki_der) {
            Ok(()) => Ok(()),
            Err(e) => Err(format!(
                "embedded responder cert not signed by issuing CA: {e}"
            )),
        };
    }

    // No embedded chain and no pinned cert: the issuing CA must have signed
    // the response directly.
    if verify_with_spki(issuer_spki_der) {
        Ok(())
    } else {
        Err("OCSP response signature does not verify against the issuing CA".to_string())
    }
}

/// POST `request_der` to `responder_url` (plain HTTP — OCSP responders are
/// conventionally unencrypted, matching `TesterOcspResponderServlet`'s
/// bare-HTTP `Connector`) and return the raw DER response body.
///
/// Hand-rolled HTTP/1.1 rather than reusing `http_client.rs`/
/// `http_url_connection.rs`: those are TLS-capable, redirect-following,
/// connection-pooling clients built for the `java.net.http`/
/// `HttpURLConnection` surface — considerably more machinery than a
/// single-shot, same-process, plain-HTTP POST needs, and neither exposes a
/// `pub` entry point at the byte-in/byte-out granularity this call wants.
fn ocsp_http_post(
    responder_url: &str,
    request_der: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let rest = responder_url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported OCSP responder URL scheme: {responder_url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let port: u16 = authority[i + 1..]
                .parse()
                .map_err(|_| format!("bad port in OCSP responder URL: {responder_url}"))?;
            (&authority[..i], port)
        }
        None => (authority, 80u16),
    };

    let addr = format!("{host}:{port}");
    // Every syscall below runs with this thread marked GC-BLOCKED, and that is
    // not an optimisation — without it this function can hang the whole VM.
    //
    // `check_ocsp` is called from certificate validation, i.e. from inside the
    // TLS handshake, on a thread the collector counts as a cooperative mutator.
    // A stop-the-world pause that begins while this thread is parked in
    // `connect`/`write`/`read` then waits for a safepoint the thread cannot
    // reach until the responder answers — `STW cross-thread JIT takeover is
    // still waiting for cooperative mutators`, repeating forever. That is
    // exactly the shape `t27_tls::gc_blocked_syscall` was written for, and this
    // was the one path still doing raw blocking socket I/O without it.
    //
    // DEFENSIVE, and labelled as such: no test in this tree is currently known
    // to fail because of it. It was added while chasing
    // `ocsp.TestOcspSoftFailInternalError`, whose log ends on exactly that
    // warning — but that turned out NOT to be this: the log never reaches an
    // OCSP fetch at all (`grep -ci ocsp` = 0), and two `/proc` samples 6 s
    // apart showed `utime` unchanged at 32 with `main-vm` in
    // `locks_lock_inode_wait`, i.e. blocked on the test fixture's own
    // `ocsp-responder.lock` flock. That class's real problem is elsewhere; see
    // `known-issues/tomcat/`.
    //
    // What justifies keeping the region anyway is the hazard class, which this
    // tree has already paid for twice: `t27_tls::gc_blocked_syscall` and
    // `net_phase_e`'s `re5` note both record a MEASURED, HotSpot-divergent hang
    // from precisely this shape — a blocking socket call on a thread the
    // collector still counts as cooperative. `ocsp_http_post` was the last
    // handshake-reachable path still doing it. The cost is one thread-state
    // flag per syscall.
    //
    // The region goes around the SYSCALL and nowhere wider. `GcBlockingSocket`
    // puts it there by construction, for the same reason it wraps rustls's
    // socket rather than rustls's exchange: a GC-blocked thread must not run
    // bytecode, and the response parsing below allocates.
    //
    // The host comes from an OCSP responder URL, i.e. text that never passed
    // through `InetAddress` — fold an IPv4-mapped destination to plain IPv4 so
    // Windows can dial it (an AF_INET6 socket cannot reach one). See
    // `outbound_policy::normalize_connect_addr`.
    let stream = {
        let _blocked = crate::t27_tls::gc_blocked_syscall();
        cratonvm_native_io::outbound_policy::connect_str_normalized(&addr)
            .map_err(|e| format!("connect {addr}: {e}"))?
    };
    // `setsockopt` does not block, so it must NOT open a region — which is what
    // the wrapper's `get_ref` is for. Set before wrapping, same effect.
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    let mut stream = crate::net_phase_e::GcBlockingSocket::new(stream);

    let mut req = Vec::with_capacity(256 + request_der.len());
    req.extend_from_slice(format!("POST {path} HTTP/1.1\r\n").as_bytes());
    req.extend_from_slice(format!("Host: {host}:{port}\r\n").as_bytes());
    req.extend_from_slice(b"Content-Type: application/ocsp-request\r\n");
    req.extend_from_slice(format!("Content-Length: {}\r\n", request_der.len()).as_bytes());
    req.extend_from_slice(b"Connection: close\r\n\r\n");
    req.extend_from_slice(request_der);

    stream.write_all(&req).map_err(|e| format!("write: {e}"))?;

    let mut all = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read: {e}")),
        }
    }

    let sep = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "OCSP HTTP response: no header terminator".to_string())?;
    let head =
        std::str::from_utf8(&all[..sep]).map_err(|e| format!("OCSP HTTP response headers: {e}"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: i32 = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!("OCSP responder returned HTTP {status}"));
    }
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    let body = &all[sep + 4..];
    if chunked {
        return decode_chunked_body(body);
    }
    match content_length {
        Some(n) if n <= body.len() => Ok(body[..n].to_vec()),
        _ => Ok(body.to_vec()),
    }
}

/// Decode an HTTP/1.1 `Transfer-Encoding: chunked` body. Tomcat (the OCSP
/// test responder) sometimes chunks small servlet responses rather than
/// pre-computing `Content-Length`.
fn decode_chunked_body(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "chunked body: bad chunk header".to_string())?;
        let size_str = std::str::from_utf8(&data[..nl]).map_err(|e| format!("chunk size: {e}"))?;
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|e| format!("chunk size: {e}"))?;
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            return Err("chunked body: truncated chunk".to_string());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.len() >= 2 && &data[..2] == b"\r\n" {
            data = &data[2..];
        }
    }
    Ok(out)
}

/// Check one certificate's revocation status via OCSP.
///
/// `issuer_subject_der`/`issuer_spki_der` describe the certificate that
/// issued `subject` (either the next chain cert, or the matched trust
/// anchor). Returns `Ok(OcspOutcome)` for any outcome this function was able
/// to determine one way or the other (including "indeterminate" reasons);
/// the only `Err` case is "no responder URL could be determined at all",
/// which callers should also treat as indeterminate/soft-failable.
fn check_ocsp(
    subject: &ParsedCert,
    issuer_subject_der: &[u8],
    issuer_spki_der: &[u8],
    revocation: &RevocationConfig,
    timeout: Duration,
) -> OcspOutcome {
    let responder_url = revocation
        .responder_uri
        .clone()
        .or_else(|| subject.ocsp_responder_uri.clone());
    let Some(responder_url) = responder_url else {
        return OcspOutcome::Indeterminate(
            "no OCSP responder URI (no PKIXRevocationChecker override and no AIA extension)"
                .to_string(),
        );
    };

    let Some(cert_id) = build_cert_id(subject, issuer_subject_der, issuer_spki_der) else {
        return OcspOutcome::Indeterminate("could not build CertID from issuer SPKI".to_string());
    };

    let request_der = build_ocsp_request(&cert_id);

    let response_der = match ocsp_http_post(&responder_url, &request_der, timeout) {
        Ok(bytes) => bytes,
        Err(e) => {
            return OcspOutcome::Indeterminate(format!(
                "OCSP request to {responder_url} failed: {e}"
            ))
        }
    };

    let parsed = match parse_ocsp_response(&response_der) {
        Ok(p) => p,
        Err(e) => {
            return OcspOutcome::Indeterminate(format!("OCSP response from {responder_url}: {e}"))
        }
    };

    let explicit_cert = revocation.responder_cert_der.as_deref();
    if let Err(e) = verify_ocsp_response_signature(&parsed, issuer_spki_der, explicit_cert) {
        return OcspOutcome::Indeterminate(format!("OCSP response signature check failed: {e}"));
    }

    let matching = parsed
        .responses
        .iter()
        .find(|r| r.cert_id_serial == cert_id.serial_der);
    match matching {
        Some(r) => match r.status {
            OcspStatusTag::Good => OcspOutcome::Good,
            OcspStatusTag::Revoked => OcspOutcome::Revoked,
            OcspStatusTag::Unknown => {
                OcspOutcome::Indeterminate("OCSP responder returned status 'unknown'".to_string())
            }
        },
        None => OcspOutcome::Indeterminate(
            "OCSP response did not include a SingleResponse for the requested serial".to_string(),
        ),
    }
}

/// Step 7 of `validate_chain`: real OCSP revocation checking, run when the
/// active `TrustManagerState` carries a `RevocationConfig`.
///
/// For every certificate in `parsed` except the trust anchor itself (a
/// trust anchor's own revocation status is not meaningful — RFC 5280
/// §6.1.1 begins the path *below* the anchor), and — when
/// `ONLY_END_ENTITY` is set — every cert except the leaf, this asks the
/// OCSP responder whether the cert is revoked.
///
/// `PREFER_CRLS`/`NO_FALLBACK`: this verifier does not implement CRL
/// fetch/parse, so both options currently collapse to "OCSP only" — see the
/// module doc / known-issues doc for this documented simplification. A
/// `NO_FALLBACK`-without-`PREFER_CRLS` configuration behaves identically to
/// the same configuration without `NO_FALLBACK`, since there is no CRL
/// fallback to suppress in the first place.
fn check_revocation(
    parsed: &[ParsedCert],
    anchor: &AnchorInfo,
    last_is_anchor: bool,
    revocation: &RevocationConfig,
) -> Result<(), TrustError> {
    let n = parsed.len();
    // Certs subject to a revocation check: everything except the trust
    // anchor itself. When the anchor is presented in-chain (last_is_anchor),
    // that final entry is excluded; otherwise every parsed cert is checked
    // (the true anchor lives outside `parsed`/`chain` entirely).
    let checked_count = if last_is_anchor {
        n.saturating_sub(1)
    } else {
        n
    };

    // 30s is generous for a same-host/LAN OCSP responder and matches the
    // ballpark of real-JDK's `com.sun.security.ocsp.timeout` default (15s) —
    // erring longer here since `SSLHostConfig.setOcspTimeout` (server side)
    // is threaded through the Tomcat-level config, not this native layer;
    // this is only the client-side (`PKIXRevocationChecker`) default.
    let timeout = Duration::from_secs(30);

    for i in 0..checked_count {
        if revocation.only_end_entity && i != 0 {
            continue;
        }
        let subject = &parsed[i];
        let (issuer_subject_der, issuer_spki_der): (&[u8], &[u8]) = if i + 1 < n {
            (
                parsed[i + 1].subject_der.as_slice(),
                parsed[i + 1].spki_der.as_slice(),
            )
        } else {
            (anchor.subject_der.as_slice(), anchor.spki_der.as_slice())
        };

        let outcome = check_ocsp(
            subject,
            issuer_subject_der,
            issuer_spki_der,
            revocation,
            timeout,
        );
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] check_revocation cert_index={} outcome={:?}",
                i, outcome
            );
        }
        match outcome {
            OcspOutcome::Good => continue,
            OcspOutcome::Revoked => {
                return Err(TrustError::Revoked { at: i });
            }
            OcspOutcome::Indeterminate(reason) => {
                if revocation.soft_fail {
                    continue;
                }
                return Err(TrustError::RevocationCheckFailed { at: i, reason });
            }
        }
    }
    Ok(())
}

/// Public entry point for `t27_tls::OcspAwareServerCertVerifier` — the native
/// `HttpURLConnection` client path's rustls verifier calls this directly
/// (rather than going through `validate_chain`, which expects raw chain DER
/// plus a full `TrustManagerState`) since it already has a parsed chain and
/// only needs the revocation step. Returns a display-formatted error string
/// rather than `TrustError` so the caller (a different module, working with
/// `rustls::Error`) doesn't need to depend on this module's error enum.
pub(crate) fn check_revocation_for_verifier(
    parsed: &[ParsedCert],
    anchor: &AnchorInfo,
    last_is_anchor: bool,
    revocation: &RevocationConfig,
) -> Result<(), String> {
    check_revocation(parsed, anchor, last_is_anchor, revocation).map_err(|e| e.to_string())
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
// as the public entry point for the layer that DOES know the peer host and the
// negotiated identification algorithm; the chain-trust path is unchanged and
// never *weakened* by this addition.
//
// Two callers thread the host in today:
//
//   * `http_url_connection::huc_verify_hostname` — the native
//     `HttpURLConnection` client path.
//   * `t27_tls::engine_check_endpoint_identity` — the `SSLEngine` lane, after
//     the handshake and after the application's `TrustManager[]` has had its
//     say (JSSE's order). This wiring was MISSING until 2026-08-03, which is
//     the whole of CVE-2018-8034's shape: a `localhost`-only certificate was
//     accepted for a connection to `127.0.0.1`. If you add a third TLS client
//     lane, it needs its own call here — an unwired lane performs no host
//     check at all, and nothing in this module can detect that.

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

pub(crate) const FQN_SUN_X509_KM: &str = "sun/security/ssl/SunX509KeyManagerImpl";
pub(crate) const FQN_X509_KM: &str = "sun/security/ssl/X509KeyManagerImpl";
pub(crate) const FQN_X509_TM: &str = "sun/security/ssl/X509TrustManagerImpl";
const FQN_PKIX_VALIDATOR: &str = "sun/security/validator/PKIXValidator";
const FQN_KMF_SUN_X509: &str = "sun/security/ssl/KeyManagerFactoryImpl$SunX509";
const FQN_TMF_SIMPLE: &str = "sun/security/ssl/TrustManagerFactoryImpl$SimpleFactory";
const FQN_TMF_PKIX: &str = "sun/security/ssl/TrustManagerFactoryImpl$PKIXFactory";

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
    // The `X509ExtendedTrustManager` overloads. `X509TrustManagerImpl` extends
    // that class, so real bytecode — and, since 2026-08-13, this crate's own
    // `t27_tls::engine_run_trust_check` — reaches these four rather than the
    // two above whenever the manager is used with an `SSLEngine` or `Socket`.
    //
    // Registering only the two-argument pair left the object half-shimmed: the
    // objects this module hands out are built with
    // `try_alloc_concurrent_synthetic`, so no constructor ever ran and every
    // instance field is null. The moment a three-argument call fell through to
    // the real JDK body it died on
    // `NullPointerException: Cannot invoke "ReentrantLock.lock()" because
    // "this.validatorLock" is null`, which the caller then reported as
    // `SSLHandshakeException: TrustManager rejected the peer certificate
    // chain` — measured on netty's `SniHandlerTest.testSniWithAlpnHandler`,
    // whose `X509TrustManagerWrapper` delegates the engine-flavoured overload
    // straight through.
    //
    // The extra `Socket`/`SSLEngine` argument is NOT advisory, and the comment
    // that used to stand here said it was. Both overloads pointed at the
    // two-argument handlers, "which ignore any surplus argument" — and the
    // surplus argument is the only thing that carries ENDPOINT IDENTIFICATION.
    // In the real `X509TrustManagerImpl` the three-argument forms read
    // `getSSLParameters().getEndpointIdentificationAlgorithm()` off it and run
    // RFC 2818 hostname verification; the two-argument form checks the chain and
    // no name. Collapsing them accepted a certificate issued for a different
    // host, measured against HotSpot in `probes/OpenSslEndpointIdentProbe.java`.
    // See `check_server_trusted_extended` for the full record.
    for desc in [
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljava/net/Socket;)V",
        "([Ljava/security/cert/X509Certificate;Ljava/lang/String;Ljavax/net/ssl/SSLEngine;)V",
    ] {
        r.register(fqn, "checkClientTrusted", desc, check_client_trusted_extended);
        r.register(fqn, "checkServerTrusted", desc, check_server_trusted_extended);
    }
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
    // "PKIX" algorithm factory — `TrustManagerFactory.getDefaultAlgorithm()`
    // is "PKIX" on modern JDKs, and Tomcat's `SSLUtilBase.getTrustManagers()`
    // uses it explicitly, calling `engineInit(ManagerFactoryParameters)`
    // whenever a CRL file or OCSP revocation checker is configured (wrapping
    // a `PKIXBuilderParameters` in a `CertPathTrustManagerParameters`), and
    // `engineInit(KeyStore)` otherwise. Real JDK dispatches BOTH overloads to
    // the methods `TrustManagerFactoryImpl` (the common base class) declares
    // — `PKIXFactory` doesn't override them — but this interpreter's native-
    // override dispatch is keyed by the RECEIVER's runtime class, so without
    // an explicit registration here `PKIXFactory` instances run the REAL
    // (unintercepted) JDK bytecode for both methods, producing a REAL
    // `X509TrustManagerImpl` whose `checkClientTrusted`/`checkServerTrusted`
    // (still natively overridden by class name — see `FQN_X509_TM`) finds no
    // `tm_registry` entry for it and silently falls back to a system-only
    // trust state, rejecting any certificate signed by a private/test CA.
    // This was invisible until `t27_tls::engine_run_trust_check` started
    // actually calling `checkClientTrusted`/`checkServerTrusted` post-
    // handshake — nothing did before.
    r.register(
        FQN_TMF_PKIX,
        "engineInit",
        "(Ljava/security/KeyStore;)V",
        tmf_engine_init,
    );
    r.register(
        FQN_TMF_PKIX,
        "engineInit",
        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
        tmf_engine_init_params,
    );
    r.register(
        FQN_TMF_PKIX,
        "engineGetTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        tmf_engine_get_trust_managers,
    );
}

/// Shared by `tmf_engine_init_params` and, cross-module,
/// `phases_late.rs`'s competing `TrustManagerFactory.init
/// (ManagerFactoryParameters)` registration (see that handler's doc
/// comment — FIX tomcat-clientauth-engine-config). Walks
/// `CertPathTrustManagerParameters.getParameters()` (real runtime type
/// `PKIXParameters`/`PKIXBuilderParameters`) -> `.getTrustAnchors()` -> each
/// `TrustAnchor.getTrustedCert()` -> `.getEncoded()` to recover the same
/// trust-anchor DER set `tmf_engine_init` gets from a plain `KeyStore` —
/// `PKIXParameters` retains no back-reference to the original `KeyStore`
/// object, so this is the only way to recover the anchors from this
/// overload. Builds and registers a `TrustManagerState` into `tm_registry`,
/// stages the pending-trust-roots/revocation thread-locals the same way
/// `tmf_engine_init` does, and returns the new `tm_registry` id — but does
/// NOT call `set_tm_id` itself, since not every caller's `this` object has
/// a safe place to land it (a `phases_late.rs`-allocated
/// `javax/net/ssl/TrustManagerFactory` has its own field-0 in active use
/// for something else entirely — writing the tm id there would corrupt
/// it). Callers own where/whether to persist the returned id.
pub(crate) fn build_and_register_tm_state_from_mfp(
    ctx: &mut dyn NativeContext,
    mfp: Option<ObjectRef>,
) -> i32 {
    let mut state = TrustManagerState::default();
    if let Some(mfp) = mfp {
        for der in extract_pkix_trust_anchor_ders(ctx, mfp) {
            insert_anchor(&mut state, der);
        }
        state.revocation = extract_revocation_config(ctx, mfp);
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] build_and_register_tm_state_from_mfp revocation_config={:?}",
                state.revocation
            );
        }
    }
    crate::t27_tls::set_pending_tm_revocation(state.revocation.clone());
    crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders.clone());
    register_trust_manager_state(state)
}

/// `engineInit(ManagerFactoryParameters)` — see `register_tmf`'s doc for why
/// this overload needs its own handler.
fn tmf_engine_init_params(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let mfp = match args.get(1) {
        Some(Value::Object(Some(mfp))) => Some(*mfp),
        _ => None,
    };
    let id = build_and_register_tm_state_from_mfp(ctx, mfp);
    set_tm_id(ctx, this, id);
    Ok(None)
}

/// Walk `CertPathTrustManagerParameters.getParameters()` (a
/// `PKIXParameters`/`PKIXBuilderParameters`) for a `PKIXCertPathChecker` that
/// is a `java.security.cert.PKIXRevocationChecker`, and extract its
/// configuration into a `RevocationConfig`. Returns `None` when there is no
/// such checker attached (the common case: `TrustManagerFactory.init` with a
/// plain `KeyStore`, or `CertPathTrustManagerParameters` without a
/// revocation checker) — `validate_chain` then skips revocation checking
/// entirely, matching real-JDK's behaviour when the caller never asked for
/// it.
///
/// Every read here goes through the real object's public API
/// (`getCertPathCheckers()`, `getOcspResponder()`, etc.) via `invoke_virtual`
/// — never raw field access — because `PKIXRevocationChecker` is a REAL
/// `java.base` object (`sun.security.provider.certpath.RevocationChecker` at
/// runtime; confirmed via `javap` against the actual JDK, not guessed), and
/// this file's own hard-learned lesson (see the identity-hash-side-table
/// pattern used elsewhere in this crate) is that field-poking a real
/// bytecode object silently no-ops or misreads. Calling its genuine getters
/// is the same "real reflection-style" approach `extract_pkix_trust_anchor_ders`
/// already uses for `getTrustAnchors()`/`getTrustedCert()`/`getEncoded()`.
fn extract_revocation_config(
    ctx: &mut dyn NativeContext,
    mfp: ObjectRef,
) -> Option<RevocationConfig> {
    let params = match ctx.invoke_virtual(
        mfp,
        "getParameters",
        "()Ljava/security/cert/CertPathParameters;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => return None,
    };
    let checkers =
        match ctx.invoke_virtual(params, "getCertPathCheckers", "()Ljava/util/List;", &[]) {
            Ok(Some(Value::Object(Some(l)))) => l,
            _ => return None,
        };
    let iter_obj = match ctx.invoke_virtual(checkers, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => return None,
    };
    loop {
        match ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => return None,
        }
        let checker = match ctx.invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(c)))) => c,
            _ => return None,
        };
        // Identify a PKIXRevocationChecker by walking the class hierarchy for
        // the well-known name, rather than assuming a specific concrete impl
        // class — `getRevocationChecker()` returns
        // `sun.security.provider.certpath.RevocationChecker` on the JDK this
        // was verified against, but the public contract only guarantees a
        // `PKIXRevocationChecker` subclass, and we'd rather match on that
        // supertype than hard-code the internal impl name.
        if !is_pkix_revocation_checker(ctx, checker) {
            continue;
        }
        return Some(read_revocation_checker_config(ctx, checker));
    }
}

/// True when `obj`'s class, or any superclass, is named
/// `java/security/cert/PKIXRevocationChecker`.
fn is_pkix_revocation_checker(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    let mut cls_id = ctx.class_id_of_object(obj);
    loop {
        match ctx.class_name_of_id(cls_id) {
            Some(name) if name == "java/security/cert/PKIXRevocationChecker" => return true,
            Some(_) => {}
            None => return false,
        }
        match ctx.superclass_of(cls_id) {
            Some(sup) => cls_id = sup,
            None => return false,
        }
    }
}

/// Read a confirmed `PKIXRevocationChecker` object's configuration via its
/// public getters: `getOcspResponder()` (URI), `getOcspResponderCert()`
/// (X509Certificate), `getOptions()` (`Set<Option>`).
fn read_revocation_checker_config(
    ctx: &mut dyn NativeContext,
    checker: ObjectRef,
) -> RevocationConfig {
    let mut cfg = RevocationConfig::default();

    if let Ok(Some(Value::Object(Some(uri)))) =
        ctx.invoke_virtual(checker, "getOcspResponder", "()Ljava/net/URI;", &[])
    {
        if let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(uri, "toString", "()Ljava/lang/String;", &[])
        {
            cfg.responder_uri = ctx.read_string(s);
        }
    }

    if let Ok(Some(Value::Object(Some(cert)))) = ctx.invoke_virtual(
        checker,
        "getOcspResponderCert",
        "()Ljava/security/cert/X509Certificate;",
        &[],
    ) {
        cfg.responder_cert_der = read_cert_der(ctx, cert);
    }

    if let Ok(Some(Value::Object(Some(options_set)))) =
        ctx.invoke_virtual(checker, "getOptions", "()Ljava/util/Set;", &[])
    {
        if let Ok(Some(Value::Object(Some(it)))) =
            ctx.invoke_virtual(options_set, "iterator", "()Ljava/util/Iterator;", &[])
        {
            loop {
                match ctx.invoke_virtual(it, "hasNext", "()Z", &[]) {
                    Ok(Some(Value::Int(1))) => {}
                    _ => break,
                }
                let opt = match ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[]) {
                    Ok(Some(Value::Object(Some(o)))) => o,
                    _ => break,
                };
                let name = match ctx.invoke_virtual(opt, "name", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                    _ => None,
                };
                match name.as_deref() {
                    Some("ONLY_END_ENTITY") => cfg.only_end_entity = true,
                    Some("PREFER_CRLS") => cfg.prefer_crls = true,
                    Some("NO_FALLBACK") => cfg.no_fallback = true,
                    Some("SOFT_FAIL") => cfg.soft_fail = true,
                    _ => {}
                }
            }
        }
    }

    cfg
}

fn extract_pkix_trust_anchor_ders(ctx: &mut dyn NativeContext, mfp: ObjectRef) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let params = match ctx.invoke_virtual(
        mfp,
        "getParameters",
        "()Ljava/security/cert/CertPathParameters;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => return out,
    };
    let anchors = match ctx.invoke_virtual(params, "getTrustAnchors", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return out,
    };
    let iter_obj = match ctx.invoke_virtual(anchors, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => return out,
    };
    loop {
        match ctx.invoke_virtual(iter_obj, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let anchor = match ctx.invoke_virtual(iter_obj, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => break,
        };
        let cert = match ctx.invoke_virtual(
            anchor,
            "getTrustedCert",
            "()Ljava/security/cert/X509Certificate;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(c)))) => c,
            _ => continue,
        };
        if let Ok(Some(Value::Object(Some(arr)))) =
            ctx.invoke_virtual(cert, "getEncoded", "()[B", &[])
        {
            let alen = ctx.array_length(arr);
            let mut der = Vec::with_capacity(alen);
            for i in 0..alen {
                if let Value::Int(b) = ctx.get_array_element(arr, i) {
                    der.push(b as u8);
                }
            }
            if !der.is_empty() {
                out.push(der);
            }
        }
    }
    out
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
    // Fallback: a REAL certificate object (e.g. `sun.security.x509.X509CertImpl`,
    // as built by `keystore::make_x509_mirror`'s preferred path) has neither of
    // the synthetic-mirror shapes above — its internal fields are the real
    // JDK's own DER-parsed representation, not a raw byte[]. Call its real
    // `getEncoded()` method (real bytecode, always present on any
    // `java.security.cert.Certificate`) to get the DER bytes instead.
    if let Ok(Some(Value::Object(Some(arr)))) = ctx.invoke_virtual(cert, "getEncoded", "()[B", &[])
    {
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

/// Identity-hash-keyed fallback table for `get_km_id`/`set_km_id` — mirrors
/// `tm_id_by_identity` (below) exactly.
///
/// FIX (client-cert-resolver): this is the "presumably the same gap" the
/// `tm_id_by_identity` doc comment predicted. `kmf_engine_init` stamps a
/// `km_id` on the `KeyManagerFactoryImpl$SunX509` instance, and
/// `kmf_engine_get_key_managers` reads it back off the SAME pointer one line
/// later — a real bytecode factory instance has no room for the
/// `cratonvm$x509km$id` pseudo-field and no `Int` at slot 0, so without this
/// fallback the round-trip silently returns 0 every time. That was invisible
/// until `chooseClientAlias`/`getPrivateKey` were actually wired into the TLS
/// handshake (see `t27_tls::JavaKeyManagerResolver`): `getPrivateKey` decodes
/// `km_id` from the returned key's packed composite, `choose_client_alias`/
/// `get_private_key` resolve `km_registry` by `get_km_id(ctx, this)` — with
/// id always 0, every lookup missed and no client certificate was ever
/// found, even though a `KeyManager` was genuinely configured.
fn km_id_by_identity() -> &'static std::sync::Mutex<std::collections::HashMap<i32, i32>> {
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, i32>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
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
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        if let Some(&id) = km_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ih)
        {
            return id;
        }
    }
    0
}

/// Trace a `KeyManager[]` (the array `SSLContext.init` was actually called
/// with) back to the identity (cert_pem, key_pem) that `chooseClientAlias`/
/// `chooseServerAlias` would ALSO pick for it, by reading each element's
/// `km_id` (set at `KeyManagerFactory.getKeyManagers` time, see `set_km_id`)
/// and looking up its `KeyManagerState`.
///
/// Returns `None` if the array is absent/empty or every element is an
/// unrecognized object (e.g. a test wrapper like Tomcat's
/// `TrackingKeyManager` that doesn't carry a `km_id`) -- callers should fall
/// back to the thread-local `PENDING_KM_IDENTITY` staging in that case, same
/// as before this function existed.
///
/// Deliberately reuses `KeyManagerState`'s ALREADY-computed
/// `client_aliases_by_key_type`/`server_aliases_by_key_type` (built by
/// `java_hashmap_iteration_order`, replicating `SunX509KeyManagerImpl`'s real
/// alias-selection order) rather than picking the keystore's first entry in
/// file order: a keystore can carry more than one otherwise-equally-eligible
/// identity (e.g. Spring Boot's own `NettyReactiveWebServerFactoryTests`
/// PKCS12 fixture, which carries a "spring-boot" and a "test-alias" client
/// identity side by side, only one of which the peer trusts -- see that
/// field's own doc comment). Picking file-order-first would silently select
/// the untrusted identity even though `chooseClientAlias` itself was already
/// fixed to pick the right one.
pub(crate) fn resolved_identity_pem_for_key_manager_array(
    ctx: &mut dyn NativeContext,
    kms_arr: Option<ObjectRef>,
) -> Option<(String, String)> {
    let arr = kms_arr?;
    let len = ctx.array_length(arr);
    let registry = km_registry().read();
    for i in 0..len {
        if let Value::Object(Some(km)) = ctx.get_array_element(arr, i) {
            let id = get_km_id(ctx, km);
            if id == 0 {
                continue;
            }
            if let Some(state) = registry.get(&id) {
                if let Some(ident) = first_identity_pem_from_state(state) {
                    return Some(ident);
                }
            }
        }
    }
    None
}

/// Pick the alias `chooseClientAlias`/`chooseServerAlias` would ALSO pick
/// (preferring a client-eligible identity, then a server-eligible one, then
/// whatever's available), and build its (cert_pem, key_pem).
fn first_identity_pem_from_state(state: &KeyManagerState) -> Option<(String, String)> {
    let alias = first_preferred_alias(state)?;
    let key_der = state.aliases_to_key.get(&alias)?;
    let chain = state.aliases_to_chain.get(&alias)?;
    Some(crate::t27_tls::der_identity_to_pem(key_der, chain))
}

fn first_preferred_alias(state: &KeyManagerState) -> Option<String> {
    for by_key_type in [
        &state.client_aliases_by_key_type,
        &state.server_aliases_by_key_type,
    ] {
        let mut key_types: Vec<&String> = by_key_type.keys().collect();
        key_types.sort();
        for kt in key_types {
            if let Some(first) = by_key_type[kt].first() {
                return Some(first.clone());
            }
        }
    }
    state.aliases_to_key.keys().next().cloned()
}

pub(crate) fn set_km_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    ctx.set_field_by_name(this, "cratonvm$x509km$id", Value::Int(id));
    let n = ctx.object_num_fields(this);
    if n > 0 {
        ctx.set_field(this, 0, Value::Int(id));
    }
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        km_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ih, id);
    }
}

/// Identity-hash-keyed fallback for `get_tm_id`/`set_tm_id` — mirrors
/// `keystore.rs::store_id_by_identity`/`get_store_id`/`set_store_id` exactly.
///
/// FIX (tls-residuals): without this, `get_tm_id`/`set_tm_id` relied solely
/// on a named pseudo-field (`cratonvm$x509tm$id`) and slot 0 — both of which
/// silently fail to round-trip on a REAL bytecode `TrustManagerFactorySpi`
/// subclass (e.g. `sun.security.ssl.TrustManagerFactoryImpl$PKIXFactory`,
/// the "real Java default" instance `TrustManagerFactory.getInstance("PKIX")`
/// actually produces): `set_field_by_name` cannot add a field a real class
/// never declared, and slot 0 (if it exists at all on the real layout) is
/// whatever field the real class puts there, not necessarily an `Int`.
/// Confirmed by direct repro: `tmf_engine_init` stamped id=1 on a `this`
/// pointer, and the VERY NEXT call to `tmf_engine_get_trust_managers` on the
/// SAME pointer read back id=0 — so the returned `TrustManager`'s
/// `checkClientTrusted`/`checkServerTrusted` (`do_check_trusted`) always
/// missed `tm_registry`, fell back to `build_trust_manager_state(0)` (~120
/// platform roots, no custom CA), and rejected any peer cert signed by that
/// CA with "no trust anchor found for chain" — even after the
/// `keystore_id_from_object`/id-unification fixes above, which only fixed
/// the KeyStore-id and tm_registry-vs-keystore-id-namespace halves of this
/// same family of bug, not this THIRD occurrence (KeyManager's
/// `cratonvm$x509km$id`/`get_km_id`/`set_km_id` a few lines up likely has the
/// identical gap, but is out of scope here — no repro hit it this session).
fn tm_id_by_identity() -> &'static std::sync::Mutex<std::collections::HashMap<i32, i32>> {
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, i32>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
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
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        if let Some(&id) = tm_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ih)
        {
            return id;
        }
    }
    0
}

pub(crate) fn set_tm_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    ctx.set_field_by_name(this, "cratonvm$x509tm$id", Value::Int(id));
    let n = ctx.object_num_fields(this);
    if n > 0 {
        ctx.set_field(this, 0, Value::Int(id));
    }
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        tm_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ih, id);
    }
}

/// Pull the `storeId` out of a Java `KeyStore` mirror.
///
/// FIX (tls-residuals): delegate to `keystore::keystore_id_from_object`,
/// which ALSO checks the identity-hash side-table `keystore.rs::set_store_id`
/// falls back to for a real `java.security.KeyStore` — this function's old
/// named-field/slot-4-only check never matched a real KeyStore wrapper
/// object (real 4-field layout, no room for a pseudo-field), so
/// `TrustManagerFactory.init(KeyStore)` always resolved id 0 for a
/// caller-supplied truststore and silently fell back to platform roots only,
/// rejecting any peer cert signed by that (private/test) CA.
fn read_keystore_id(ctx: &mut dyn NativeContext, ks: ObjectRef) -> i32 {
    crate::keystore::keystore_id_from_object(ctx, ks)
}

/// FIX (TestManagerWebappSsl sslConnectorCerts, "Subject: CN=..." missing from
/// the manager's cert-chain listing): this used to always
/// `alloc_concurrent_synthetic` a BARE `java/security/cert/X509Certificate`
/// (the abstract class itself, which has no `toString()` implementation of
/// its own) and stash `alias` as both subject and issuer, unconditionally —
/// a local, worse duplicate of `keystore::make_x509_mirror`, which this same
/// file's own DER-extraction fallback above already documents as the
/// "preferred path" for a REAL certificate object. Calling `.toString()` on
/// the bare synthetic fell through to `Object.toString()`
/// (`java.security.cert.X509Certificate@<hash>`), not a real
/// subject/issuer/validity dump — breaking `ManagerServlet.sslConnectorCerts`
/// (`cert.toString()`) and anything else relying on a real
/// `X509Certificate.toString()`/`checkValidity()`/etc. Delegate to
/// `keystore::make_x509_mirror` instead, which tries a REAL
/// `sun.security.x509.X509CertImpl` (real bytecode, so `toString()` and
/// friends work correctly) parsed from the DER first, only falling back to
/// a bare synthetic mirror if that construction itself fails.
fn make_x509_mirror(ctx: &mut dyn NativeContext, alias: &str, der: &[u8]) -> Result<ObjectRef, MethodCallFailed> {
    Ok(crate::keystore::make_x509_mirror(ctx, alias, der)?)
}

fn make_private_key_mirror(
    ctx: &mut dyn NativeContext,
    key_der: &[u8],
    km_id: i32,
    alias: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let pk = try_alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 4)?;
    // Same packing convention keystore.rs uses so the TLS path can decode
    // the (km_id, alias_hash) pair — plus [`KM_PROXY_TAG`], which says WHICH
    // registry the high half indexes. Without the tag the two conventions are
    // indistinguishable and `keystore::private_key_der_from_proxy` read this
    // `km_id` as a KEYSTORE id.
    let alias_hash = fnv1a_32(alias.as_bytes());
    let composite =
        KM_PROXY_TAG | ((km_id as i64 & 0xFFFF_FFFF) << 32) | (alias_hash as i64 & 0xFFFF_FFFF);
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
    Ok(pk)
}

/// Decode the `km_id` half of the `(km_id, alias_hash)` composite
/// `make_private_key_mirror` packs into field 3 of a synthetic `PrivateKey`
/// mirror. Used by `t27_tls::JavaKeyManagerResolver::resolve` after calling
/// the real (possibly test-wrapped) `KeyManager.getPrivateKey(alias)` via
/// `ctx.invoke_virtual`: the caller already knows `alias` (it chose it), so
/// only `km_id` needs recovering from the returned mirror before looking the
/// actual DER material up via [`km_alias_material`]. Returns `None` if `pk`
/// isn't a field-3-Long-composite mirror (e.g. `getPrivateKey` returned
/// `null` or a differently-shaped object).
pub(crate) fn km_id_from_private_key_mirror(ctx: &dyn NativeContext, pk: ObjectRef) -> Option<i32> {
    match ctx.get_field(pk, 3) {
        Value::Long(composite) => Some(km_id_of_composite(composite)),
        _ => None,
    }
}

/// Marks a four-slot `java/security/PrivateKey` proxy whose field-3 composite
/// indexes [`km_registry`] — a `KeyManager` id — rather than the keystore
/// registry that `keystore::private_key_der_from_proxy` was written for.
///
/// Both producers pack `(id << 32) | alias_hash` into the same slot of the same
/// class, and the two id spaces are independent counters, so the reader had no
/// way to tell them apart and always chose the keystore. That is not a
/// hypothetical: `KeyManager.getPrivateKey(alias).getEncoded()` returned the
/// right DER only while `km_id` happened to name a live keystore holding an
/// alias with the same FNV hash, and an EMPTY array as soon as the counters
/// drifted apart. netty's `OpenSslCachingX509KeyManagerFactory.newProvider`
/// calls `getKeyManagers()` twice, which was enough to drift them: every
/// `chooseKeyMaterial` after it built a PEM with no body, and BoringSSL's
/// `PEM_read_bio_PrivateKey` returned NULL without queueing an error, so
/// tcnative reported the uninformative `Unable to load certificate key
/// (error:00000000:invalid library (0))`.
///
/// Bit 62: no id counter reaches it, and it leaves the value positive so the
/// `Long` round-trips through Java unchanged.
const KM_PROXY_TAG: i64 = 1 << 62;

/// The `km_id` half of a composite, with [`KM_PROXY_TAG`] removed.
pub(crate) fn km_id_of_composite(composite: i64) -> i32 {
    ((composite & !KM_PROXY_TAG) >> 32) as i32
}

/// Is this field-3 composite one of `make_private_key_mirror`'s — i.e. does its
/// high half index [`km_registry`] rather than the keystore registry?
pub(crate) fn is_km_proxy_composite(composite: i64) -> bool {
    composite & KM_PROXY_TAG != 0
}

/// The PKCS#8 DER `km_id` holds for the alias whose FNV-1a hash is
/// `alias_hash`. The proxy carries only the hash, so the alias is recovered by
/// scanning this manager's own alias set — the same shape
/// `keystore::private_key_der_from_proxy` uses, and bounded by the number of
/// entries one `KeyManagerFactory` was initialised with.
pub(crate) fn km_key_der_by_alias_hash(km_id: i32, alias_hash: u32) -> Option<Vec<u8>> {
    let registry = km_registry().read();
    let state = registry.get(&km_id)?;
    state
        .aliases_to_key
        .iter()
        .find(|(alias, _)| fnv1a_32(alias.as_bytes()) == alias_hash)
        .map(|(_, der)| der.clone())
}

/// Look up the DER cert chain (leaf first) and PKCS#8 private-key DER
/// registered for `alias` under `km_id` in `km_registry`. Used by
/// `t27_tls::JavaKeyManagerResolver::resolve` once it has an alias (from
/// `chooseClientAlias`) and a `km_id` (decoded from `getPrivateKey`'s
/// returned mirror via [`km_id_from_private_key_mirror`]) — this reads the
/// same registry `choose_client_alias`/`get_certificate_chain`/
/// `get_private_key` already consult, so it stays consistent with whatever
/// `chooseClientAlias` actually picked, without a second round of native
/// method calls into Java to fetch the chain/key material.
pub(crate) fn km_alias_material(km_id: i32, alias: &str) -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    let registry = km_registry().read();
    let state = registry.get(&km_id)?;
    let chain = state.aliases_to_chain.get(alias)?.clone();
    let key = state.aliases_to_key.get(alias)?.clone();
    Some((chain, key))
}

fn classify_key_type_from_pkcs8(key_der: &[u8]) -> &'static str {
    // Structural read first: walk PrivateKeyInfo to its AlgorithmIdentifier OID
    // rather than searching the whole DER for OID bytes, which can also match
    // key material that happens to contain them. The needle scan below stays as
    // the fallback for encodings that walk fails on, and "RSA" remains the
    // last-resort default this function has always returned -- callers here
    // treat it as a hint, not a verdict.
    if let Some(name) = crate::t27_tls::pkcs8_algorithm_name(key_der) {
        return name;
    }
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

/// Throw a REAL `java.security.cert.CertificateException` for a failed PKIX
/// check, rather than an `IOException` whose *message* merely names one.
///
/// `X509TrustManager.checkServerTrusted`/`checkClientTrusted` declare
/// `throws CertificateException`, and **callers discriminate on the TYPE**:
/// a TLS stack catches `CertificateException` to turn a validation failure
/// into a handshake alert, and a test catches it to assert that an untrusted
/// chain was in fact rejected. An `IOException` matches neither, so it sails
/// straight through the `catch` that exists to handle exactly this.
///
/// `io.netty.handler.ssl.SslContextTrustManagerTest` is the witness: its two
/// mixed-expectation tests (`testUsingCAsOneAandB`, `testUsingCAsOneAandTwo`)
/// call `checkServerTrusted` inside `catch (CertificateException)` and assert
/// the negative case was rejected. With an `IOException` the negative case
/// escaped the catch and failed the test, while the two all-positive tests
/// passed — so the symptom looked like "some chains do not validate" when the
/// validation verdict was right and only its exception class was wrong.
///
/// Falls back to the historic `IOException` when the class cannot be
/// constructed, so no configuration loses the failure entirely — the one
/// thing that must never happen here is a silently-trusted connection.
fn cert_exception(ctx: &mut dyn NativeContext, message: String) -> MethodCallFailed {
    cert_exception_of(ctx, "java/security/cert/CertificateException", message)
}

/// [`cert_exception`] for a specific exception class — `CertPathValidatorException`
/// on the `PKIXValidator.engineValidate` path, which declares that type rather
/// than `CertificateException`.
/// `pub(crate)` re-export of [`cert_exception`] for `tls.rs`'s
/// `checkServerTrusted` path, so both trust-check entry points raise the same
/// exception CLASS rather than agreeing only on the message text.
pub(crate) fn cert_exception_external(
    ctx: &mut dyn NativeContext,
    message: String,
) -> MethodCallFailed {
    cert_exception(ctx, message)
}

fn cert_exception_of(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    message: String,
) -> MethodCallFailed {
    let msg = ctx.create_string(&message);
    match ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IOException { message }.into(),
    }
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
        ctx.set_array_element(arr, i, Value::Object(Some(mirror?)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn get_private_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);
    let alias = read_string_at(ctx, args, 1).unwrap_or_default();

    // A key with no encoding of its own is served BY REFERENCE — the caller
    // asked for the `PrivateKey` object, and for an opaque key (PKCS#11, or
    // netty's `OpenSslPrivateKey`) that object is the only usable answer. See
    // `KeyManagerState::aliases_to_live_key`.
    if let Some(live) = km_registry()
        .read()
        .get(&id)
        .and_then(|s| s.aliases_to_live_key.get(&alias))
        .copied()
    {
        return Ok(Some(Value::Object(Some(live))));
    }
    let key_der = {
        let registry = km_registry().read();
        match registry.get(&id).and_then(|s| s.aliases_to_key.get(&alias)) {
            Some(k) => k.clone(),
            None => return Ok(Some(Value::Object(None))),
        }
    };
    let pk = make_private_key_mirror(ctx, &key_der, id, &alias);
    Ok(Some(Value::Object(Some(pk?))))
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

/// Build a Java `String[]` from `items`.
///
/// GC NOTE, and it is the whole reason this is not three lines. `arr` outlives
/// `create_string`, which ALLOCATES: under a moving young collector the array
/// is relocated by that allocation and every `set_array_element` after it
/// writes into the vacated slots. What the live array keeps is whatever the
/// collector left there — usually `null`.
///
/// This is the same defect the `openssl-key-material-and-engine-residuals`
/// write-up (now retired) recorded in its §D against
/// `getAcceptedIssuers`, at the two methods it did NOT sweep:
/// `getServerAliases` and `getClientAliases`. A null-riddled alias array is
/// exactly what netty's `OpenSslKeyMaterialProvider` turns into
/// `NO_CERTIFICATE_SET` / `Unable to find key material for auth method(s)`,
/// and it is intermittent for the same reason every instance of this shape is:
/// it needs a collection to land inside the loop.
///
/// `t27_tls::build_issuer_principals` has carried the rooted form since
/// 2026-08-01; this is that form.
fn materialize_string_array(ctx: &mut dyn NativeContext, items: &[String]) -> ObjectRef {
    let cls_id = ctx
        .ensure_class_initialized("java/lang/String")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, items.len());
    let arr_h = scope.root(arr);
    for (i, s) in items.iter().enumerate() {
        let js = scope.create_string(s);
        let arr = scope.get(&arr_h);
        scope.set_array_element(arr, i, Value::Object(Some(js)));
    }
    scope.get(&arr_h)
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

/// `X509ExtendedTrustManager.checkServerTrusted(chain, authType, Socket|SSLEngine)`.
///
/// **This is NOT the two-argument check with a spare argument.** In the real
/// `sun.security.ssl.X509TrustManagerImpl` the three-argument overloads are the
/// only place ENDPOINT IDENTIFICATION — RFC 2818 hostname verification — runs:
/// `checkTrusted` reads
/// `engine.getSSLParameters().getEndpointIdentificationAlgorithm()` and, when it
/// is non-empty, calls `checkIdentity`, which ends in
/// `HostnameChecker.getInstance(TYPE_TLS).match(...)`. The two-argument overload
/// deliberately checks the CHAIN and no name at all.
///
/// This module used to register one handler for all three descriptors, with a
/// comment calling the extra argument "advisory". It is not, and the cost was a
/// silent hole: **a certificate issued for a different host was accepted.**
/// MEASURED, `probes/OpenSslEndpointIdentProbe.java`, with every input printed
/// identical on both VMs (`endpointIdentificationAlgorithm=HTTPS`,
/// `peerHost=localhost`, an extended handshake session, peer subject
/// `CN=NOTlocalhost`):
///
/// ```text
/// HotSpot  @@TM delegate=REJECTED CertificateException: No name matching localhost found
/// CratonVM @@TM delegate=ACCEPTED (no CertificateException)
/// ```
///
/// netty's OpenSSL provider is one such caller: BoringSSL hands the chain back
/// to Java and `ReferenceCountedOpenSslClientContext.ExtendedTrustManagerVerifyCallback`
/// calls exactly this overload, so all 48 parameterisations of
/// `SSLEngineTest.testClientHostnameValidationFail` completed a handshake that
/// HotSpot rejects, in three netty SSL classes. `probes/HostnameCheckerProbe.java`
/// clears the JDK's own checker of any part in it: called directly it answers
/// `No name matching localhost found` on this VM too — it was simply never
/// reached. See
/// `fixed-suite-bugs/netty/ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`.
fn check_server_trusted_extended(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    do_check_trusted(ctx, args)?;
    check_extended_tm_endpoint_identity(ctx, args, false)
}

/// The client-authentication twin of [`check_server_trusted_extended`].
///
/// The JDK runs `checkIdentity` in this direction too, and turns a failure into
/// `CertificateException("Endpoint Identification Algorithm HTTPS is not
/// supported on the server side")` rather than the name mismatch — because a
/// server has no name to identify its client by. Mirrored here rather than
/// skipped, so a server that DOES set the algorithm gets the same answer it
/// gets on HotSpot instead of a silent pass.
fn check_client_trusted_extended(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    do_check_trusted(ctx, args)?;
    check_extended_tm_endpoint_identity(ctx, args, true)
}

/// What the third argument of an `X509ExtendedTrustManager` overload is.
///
/// A plain `java.net.Socket` that is not an `SSLSocket` carries no SSL
/// parameters and no handshake session — the JDK guards on
/// `socket instanceof SSLSocket` and skips identification entirely — so it is a
/// third case, not an error.
enum ExtendedTmPeer {
    Engine,
    SslSocket,
}

fn classify_extended_tm_peer(
    ctx: &mut dyn NativeContext,
    peer: ObjectRef,
) -> Option<ExtendedTmPeer> {
    let cid = ctx.class_id_of_object(peer);
    for (name, kind) in [
        ("javax/net/ssl/SSLEngine", ExtendedTmPeer::Engine),
        ("javax/net/ssl/SSLSocket", ExtendedTmPeer::SslSocket),
    ] {
        if let Ok(target) = ctx.ensure_class_initialized(name) {
            if cid == target || ctx.is_subclass(cid, target) {
                return Some(kind);
            }
        }
    }
    None
}

/// `x.getSSLParameters().getEndpointIdentificationAlgorithm()`, or `None` when
/// the peer has none (which means "do not identify", exactly as in the JDK).
fn extended_tm_identification_algorithm(
    ctx: &mut dyn NativeContext,
    peer: ObjectRef,
) -> Option<String> {
    let params = match ctx.invoke_virtual(
        peer,
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => return None,
    };
    match ctx.invoke_virtual(
        params,
        "getEndpointIdentificationAlgorithm",
        "()Ljava/lang/String;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// The handshake session, if the peer has one.
///
/// The JDK reads the host off the HANDSHAKE session rather than off the engine,
/// and refuses the whole check outright when there is none
/// (`CertificateException: No handshake session`). That last part is
/// deliberately NOT reproduced — see `extended_tm_peer_host`.
fn extended_tm_handshake_session(
    ctx: &mut dyn NativeContext,
    peer: ObjectRef,
) -> Option<ObjectRef> {
    match ctx.invoke_virtual(
        peer,
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(s)))) => Some(s),
        _ => None,
    }
}

fn extended_tm_session_peer_host(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Option<String> {
    match ctx.invoke_virtual(session, "getPeerHost", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// The host to identify the peer by: the handshake session's, then the
/// engine's/socket's own.
///
/// The JDK reads only the session's, because on HotSpot the session is always
/// there by the time a trust manager runs. The second source is what lets this
/// VM decline to throw when it is not — `SSLEngine.getPeerHost()` is the value
/// netty passed to `newHandler(alloc, host, port)` and is the same string the
/// session would have reported.
fn extended_tm_peer_host(
    ctx: &mut dyn NativeContext,
    peer: ObjectRef,
    session: Option<ObjectRef>,
) -> Option<String> {
    if let Some(s) = session {
        if let Some(h) = extended_tm_session_peer_host(ctx, s).filter(|h| !h.is_empty()) {
            return Some(h);
        }
    }
    match ctx.invoke_virtual(peer, "getPeerHost", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|h| !h.is_empty()),
        _ => None,
    }
}

/// The `host_name` entry of the session's requested SNI names, ASCII form.
///
/// `X509TrustManagerImpl.checkIdentity` prefers this over the peer host — "the
/// server_name extension is more reliable than peer host", says its own comment
/// — and falls back to the peer host when the SNI check fails against a
/// DIFFERENT name. Reproducing the preference matters: a client that connects by
/// IP but sends SNI is identified by the SNI name on HotSpot.
fn extended_tm_sni_host_name(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Option<String> {
    let names = match ctx.invoke_virtual(
        session,
        "getRequestedServerNames",
        "()Ljava/util/List;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => return None,
    };
    let size = match ctx.invoke_virtual(names, "size", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        _ => return None,
    };
    for i in 0..size {
        let Ok(Some(Value::Object(Some(sn)))) =
            ctx.invoke_virtual(names, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])
        else {
            continue;
        };
        // Type 0 is `StandardConstants.SNI_HOST_NAME`; only that one carries a
        // host name, and only `SNIHostName` declares `getAsciiName()`.
        if !matches!(ctx.invoke_virtual(sn, "getType", "()I", &[]), Ok(Some(Value::Int(0)))) {
            continue;
        }
        if let Ok(Some(Value::Object(Some(s)))) =
            ctx.invoke_virtual(sn, "getAsciiName", "()Ljava/lang/String;", &[])
        {
            if let Some(text) = ctx.read_string(s).filter(|t| !t.is_empty()) {
                return Some(text);
            }
        }
    }
    None
}

/// `X509TrustManagerImpl.checkIdentity`, re-derived over this module's own
/// `verify_hostname` so the two identity checks in this VM cannot drift apart.
///
/// The ORDER is the JDK's and is load-bearing: try the SNI name first; if that
/// passes, done; if it fails and the SNI name IS the peer host, fail with that;
/// otherwise fall back to the peer host. Checking only the peer host would
/// reject connections HotSpot accepts (SNI set, connected by IP), and checking
/// only SNI would accept ones it rejects.
fn check_extended_tm_endpoint_identity(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    checking_client: bool,
) -> MethodCallResult {
    let Some(peer) = (match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    }) else {
        // The JDK validates the chain and skips identification when the extra
        // argument is null. So do we — `do_check_trusted` already ran.
        return Ok(None);
    };
    if classify_extended_tm_peer(ctx, peer).is_none() {
        return Ok(None);
    }
    let Some(algorithm) = extended_tm_identification_algorithm(ctx, peer) else {
        return Ok(None);
    };
    let session = extended_tm_handshake_session(ctx, peer);

    let chain_val = match args.get(1) {
        Some(v) => v.clone(),
        None => Value::Object(None),
    };
    let chain = read_chain_arg(ctx, &chain_val);
    let Some(leaf_der) = chain.first() else {
        return Err(cert_exception(ctx, "certificate chain is empty".into()));
    };
    let leaf = match parse_certificate(leaf_der) {
        Ok(c) => c,
        Err(e) => {
            return Err(cert_exception(
                ctx,
                format!("peer certificate could not be parsed: {e}"),
            ))
        }
    };

    // Only "HTTPS" (and the LDAP spellings, which use the same RFC 2818 name
    // matching in this VM) identify by name; anything else is an error on
    // HotSpot rather than a silent pass.
    let known = algorithm.eq_ignore_ascii_case("HTTPS")
        || algorithm.eq_ignore_ascii_case("LDAP")
        || algorithm.eq_ignore_ascii_case("LDAPS");
    if !known {
        return Err(cert_exception(
            ctx,
            format!("Unknown identification algorithm: {algorithm}"),
        ));
    }

    let peer_host = extended_tm_peer_host(ctx, peer, session).map(|h| {
        // An FQDN's trailing dot is not allowed in an SNIHostName and is not
        // part of the name a certificate asserts.
        h.strip_suffix('.').unwrap_or(&h).to_string()
    });
    let sni_host = match (checking_client, session) {
        (false, Some(s)) => extended_tm_sni_host_name(ctx, s),
        _ => None,
    };

    if let Some(sni) = sni_host.as_deref() {
        match verify_hostname(&leaf, sni) {
            Ok(()) => return Ok(None),
            Err(e) => {
                if peer_host
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(sni))
                {
                    return Err(cert_exception(ctx, e.to_string()));
                }
                // otherwise fall through to the peer host, as the JDK does
            }
        }
    }

    let Some(host) = peer_host else {
        // NOT the JDK's `CertificateException: Hostname or IP address is
        // undefined.`, and the difference is deliberate. On HotSpot the engine
        // reaching this point always has a handshake session carrying the host;
        // in this VM the same overload is ALSO called by `t27_tls`'s own rustls
        // engine, whose `SSLEngine` object need not carry one, and that path
        // performs endpoint identification itself
        // (`t27_tls::jsse_owns_endpoint_identification` -> `engine_check_endpoint_identity`).
        // Throwing here would break every TLS handshake that VM stack makes in
        // order to close a hole it does not have. Recorded rather than silent.
        tracing::debug!(
            "endpoint identification requested ({algorithm}) but the peer reports no host;              skipping the name check"
        );
        return Ok(None);
    };
    match verify_hostname(&leaf, &host) {
        Ok(()) => Ok(None),
        Err(e) => {
            if checking_client && algorithm.eq_ignore_ascii_case("HTTPS") {
                Err(cert_exception(
                    ctx,
                    "Endpoint Identification Algorithm HTTPS is not supported on the server side"
                        .into(),
                ))
            } else {
                Err(cert_exception(ctx, e.to_string()))
            }
        }
    }
}

fn do_check_trusted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Publish this thread's native context so the OCSP fetch far below can
    // mark the thread GC-BLOCKED around its socket syscalls.
    //
    // Without this the VM DEADLOCKS, and the path is not the obvious one.
    // `check_revocation` -> `check_ocsp` -> `ocsp_http_post` blocks in `recv`
    // on the OCSP responder. `ocsp_http_post` wraps its socket in
    // `GcBlockingSocket`, but that wrapper calls `gc_blocked_syscall()`, which
    // reads a thread-local published by `set_active_native_context` and
    // degrades to an INERT guard when none is. Both existing publishers are on
    // the HTTPS **client** path (`http_url_connection::perform`, and
    // `net_phase_e`'s raw-socket path). This is the **server** side —
    // `checkClientTrusted`, the server validating the client's certificate —
    // so no context was ever published and the wrapper did nothing.
    //
    // MEASURED with `sudo gdb -p` on `ocsp.TestOcspSoftFailInternalError`,
    // which stalls at a 900 s cap where HotSpot passes 20 tests. The VM logs
    // `STW cross-thread JIT takeover is still waiting for cooperative mutators
    // rounds=64 pending=1 taken=0` — exactly one uncooperative thread — and
    // that thread's stack is:
    //
    // ```text
    //   check_client_trusted -> do_check_trusted -> validate_chain
    //     -> validate_ordered_chain -> check_revocation -> check_ocsp
    //       -> ocsp_http_post -> GcBlockingSocket::read -> recv(fd=19)
    // ```
    //
    // It is parked in `recv` while the collector still counts it as a
    // cooperative mutator, so the stop-the-world request can never be
    // satisfied: the thread is neither in JIT code (cannot be taken over) nor
    // at a safepoint (cannot cooperate). Every other thread — including the
    // client half of the very exchange that responder answer belongs to — is
    // parked at that barrier behind it.
    //
    // Publishing does NOT mark the thread blocked; it only makes the region
    // `GcBlockingSocket` opens around each syscall reachable. This function
    // runs Java (`invoke_virtual` on the delegate `TrustManager`, allocations),
    // which a blocked thread must never do — and does not have to, because the
    // region covers the syscall and nothing wider. Same split
    // `t27_tls::gc_blocked_syscall` documents for rustls.
    //
    // The guard save/restores rather than clears (see
    // `ActiveNativeContextGuard`): this is the second publisher, and on the
    // client path `perform` has already published one that must survive.
    let _active_ctx = crate::t27_tls::set_active_native_context(ctx);
    let this = this_arg(args)?;
    let id = get_tm_id(ctx, this);
    let chain_val = match args.get(1) {
        Some(v) => v.clone(),
        None => Value::Object(None),
    };
    let chain = read_chain_arg(ctx, &chain_val);
    if chain.is_empty() {
        return Err(cert_exception(ctx, "certificate chain is empty".into()));
    }

    let (trust, hit) = {
        let registry = tm_registry().read();
        match registry.get(&id) {
            Some(s) => (s.clone(), true),
            None => {
                // Fallback to a system-only trust state — better than denying
                // everything when init() was bypassed (which real-JDK permits
                // for the implicit default trust manager).
                drop(registry);
                (build_trust_manager_state(0), false)
            }
        }
    };
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] do_check_trusted id={} registry_hit={} anchor_ders={} anchors_groups={} chain_len={}",
            id,
            hit,
            trust.anchor_ders.len(),
            trust.anchors.len(),
            chain.len()
        );
    }

    match validate_chain(&chain, &trust) {
        Ok(()) => Ok(None),
        Err(e) => Err(cert_exception(ctx, e.to_string())),
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
    let arr0 = ctx.new_ref_array(cls_id, ders.len());
    // GC: `make_x509_mirror` allocates (a mirror object, its strings, its DER
    // byte[]), so a moving young collection can relocate `arr` INSIDE this
    // loop. Held raw, every `set_array_element` after that point wrote into
    // the vacated slots and the live array kept whatever the collector put
    // there — nulls, or unrelated objects the mirror building itself made.
    //
    // netty saw both faces of it on one call site
    // (`ReferenceCountedOpenSslServerContext.newSessionContext` →
    // `toBIO(alloc, manager.getAcceptedIssuers())`): intermittently
    // `IllegalArgumentException: Null element in chain: [null × 32]`, and
    // intermittently `NoSuchMethodError: sun.security.util.DerValue
    // .getEncoded()` — a `DerValue` left in a vacated slot by the very
    // certificate parsing this loop had just done. Same family as
    // `t27_tls::attach_trust_managers_to_ctx`'s documented GC fix: a native
    // local held live across an allocation.
    let pin = ctx.pin_native_root(arr0);
    let mut arr = arr0;
    let mut failure = None;
    for (i, der) in ders.iter().enumerate() {
        match make_x509_mirror(ctx, "trust-anchor", der) {
            Ok(mirror) => {
                arr = ctx.read_native_pin(pin, arr0);
                ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    if let Some(e) = failure {
        return Err(e);
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
        None => return Err(cert_exception(ctx, "null chain".into())),
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
        Err(e) => Err(cert_exception_of(
            ctx,
            "java/security/cert/CertPathValidatorException",
            e.to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// KeyManagerFactory / TrustManagerFactory
// ---------------------------------------------------------------------------

fn kmf_engine_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    // The provider-SPI route is selected by real-JDK SunJSSE. Mirror the
    // public KeyManagerFactory.init shim so a JKS key whose password differs
    // from its store password is materialized before SSLContext initialization.
    if let Some(Value::Object(Some(ks))) = args.get(1) {
        let key_password = args
            .get(2)
            .map(|value| crate::keystore::read_password(ctx, value))
            .unwrap_or_default();
        if crate::nbflags().dbg_tls_auth {
            eprintln!(
                "[dbg-tls-auth] kmf_engine_init this_ptr={:?} ks_id={} password_len={}",
                this.as_ptr(),
                read_keystore_id(ctx, *ks),
                key_password.len()
            );
        }
        crate::keystore::keystore_set_pending_km_identity_with_password(ctx, *ks, &key_password);
    }
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

/// Which `KeyManager` implementation class a `KeyManagerFactory` of this
/// algorithm hands out.
///
/// The JDK has two, and the difference is observable: `SunX509` yields
/// `sun.security.ssl.SunX509KeyManagerImpl`, while `PKIX` and its alias
/// `NewSunX509` yield `sun.security.ssl.X509KeyManagerImpl`. CratonVM returned
/// the `SunX509` class for every algorithm.
///
/// This is not cosmetic. Netty's `OpenSslCachingX509KeyManagerFactory.newProvider`
/// branches on exactly this class name — `X509KeyManagerImpl` means "aliases are
/// not stable and will change between invocations", so it must NOT cache key
/// material against them. Reporting the `SunX509` class for a PKIX factory made
/// netty cache where the JDK's own contract says it may not.
///
/// Both classes are already fully registered by `register_key_manager`, so this
/// only decides which one to stamp on the mirror.
pub(crate) fn km_mirror_class_for_algorithm(algorithm: &str) -> &'static str {
    if algorithm.eq_ignore_ascii_case("PKIX") || algorithm.eq_ignore_ascii_case("NewSunX509") {
        FQN_X509_KM
    } else {
        FQN_SUN_X509_KM
    }
}

fn kmf_engine_get_key_managers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_km_id(ctx, this);

    // Return a 1-element KeyManager[] holding the mirror class this factory's
    // algorithm calls for. On the SPI route the algorithm is not a field — it
    // is the SPI's own class (`KeyManagerFactoryImpl$SunX509` vs `$X509`), so
    // read it from there.
    let spi_class = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    let mirror = if spi_class.ends_with("$X509") || spi_class.ends_with("$PKIX") {
        FQN_X509_KM
    } else {
        FQN_SUN_X509_KM
    };
    let cls_id = ctx
        .ensure_class_initialized("javax/net/ssl/KeyManager")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    // GC NOTE: `try_alloc_concurrent_synthetic` allocates, so the array must be
    // rooted across it — see `materialize_string_array`. A length-1 array is
    // not exempt: the relocation moves the array, not the element count, and a
    // `KeyManager[]` whose only slot reads back `null` is
    // `getKeyManagers()[0]` throwing where the caller cannot see why.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, 1);
    let arr_h = scope.root(arr);
    let km = try_alloc_concurrent_synthetic(&mut *scope, mirror, 2)?;
    let km_h = scope.root(km);
    let km = scope.get(&km_h);
    set_km_id(&mut *scope, km, id);
    let km = scope.get(&km_h);
    let arr = scope.get(&arr_h);
    scope.set_array_element(arr, 0, Value::Object(Some(km)));
    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

fn tmf_engine_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let ks_id = match args.get(1) {
        Some(Value::Object(Some(ks))) => read_keystore_id(ctx, *ks),
        _ => 0,
    };
    let state = build_trust_manager_state(ks_id);
    crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders.clone());
    let id = register_trust_manager_state(state);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] tmf_engine_init this_ptr={:?} ks_id={} new_tm_id={}",
            this.as_ptr(),
            ks_id,
            id
        );
    }
    set_tm_id(ctx, this, id);
    Ok(Some(Value::Object(None)))
}

fn tmf_engine_get_trust_managers(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_tm_id(ctx, this);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] tmf_engine_get_trust_managers this_ptr={:?} read_id={}",
            this.as_ptr(),
            id
        );
    }

    let cls_id = ctx
        .ensure_class_initialized("javax/net/ssl/TrustManager")
        .unwrap_or(cratonvm_types::ClassId::new(0));
    // GC NOTE: same rooting as `kmf_engine_get_key_managers` above.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(cls_id, 1);
    let arr_h = scope.root(arr);
    let tm = try_alloc_concurrent_synthetic(&mut *scope, FQN_X509_TM, 2)?;
    let tm_h = scope.root(tm);
    let tm = scope.get(&tm_h);
    set_tm_id(&mut *scope, tm, id);
    let tm = scope.get(&tm_h);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] tmf_engine_get_trust_managers stamped tm_ptr={:?} id={}",
            tm.as_ptr(),
            id
        );
    }
    let arr = scope.get(&arr_h);
    scope.set_array_element(arr, 0, Value::Object(Some(tm)));
    Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Hermetic tests built against a hand-rolled X.509 fixture. We never
    //! shell out to `openssl` — every cert byte here is produced by the
    //! `mk_cert` builder below so the suite passes on any host.
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};

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
    fn server_cert_prefers_eku_serverauth_when_eku_present() {
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
    fn client_cert_prefers_eku_clientauth_when_eku_present() {
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
    fn cert_without_eku_is_preferred_for_both_roles() {
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

    /// An EKU that names only `serverAuth` must still yield a CLIENT alias.
    ///
    /// `SunX509KeyManagerImpl` — the default `KeyManagerFactory` algorithm —
    /// filters aliases on the key algorithm and the peer's issuer list only, so
    /// on JDK 25 this exact certificate answers `chooseClientAlias(RSA)=key`.
    /// Treating [`is_client_cert`] as an eligibility test instead of a
    /// preference made `getClientAliases(RSA)` answer `[]` and
    /// `chooseClientAlias(RSA)` answer `null`, so netty's OPENSSL client sent no
    /// certificate at all and a `ClientAuth.REQUIRE` server closed the
    /// handshake with `SSLV3_ALERT_HANDSHAKE_FAILURE` — 14 of the 17 residual
    /// rows of `JdkDelegatingPrivateKeyMethodTest`, whose fixture builds
    /// `.setKeyUsage(true, digitalSignature).addExtendedKeyUsageServerAuth()`
    /// and then uses that one cert on BOTH sides.
    #[test]
    fn an_eku_mismatch_orders_an_alias_last_but_never_drops_it() {
        let server_only = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "serveronly.example",
            issuer_cn: "serveronly.example",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let both = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "both.example",
            issuer_cn: "both.example",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH, OID_KP_CLIENT_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });

        // A store holding ONLY the serverAuth-marked cert: the client list must
        // still name it. This is the shape the netty fixture builds.
        let mut only = crate::keystore::LoadedKeyStore::default();
        only.entries.insert(
            "key".to_string(),
            crate::keystore::KeyStoreEntry {
                alias: "key".to_string(),
                creation_time_ms: 0,
                kind: crate::keystore::EntryKind::PrivateKey {
                    key_der: vec![0x30, 0x00],
                    chain: vec![server_only.clone()],
                },
            },
        );
        let st = build_key_manager_state(crate::keystore::keystore_register(only));
        assert_eq!(
            st.client_aliases_by_key_type.get("RSA").map(Vec::as_slice),
            Some(&["key".to_string()][..]),
            "a serverAuth-only cert must still be offered as a client alias"
        );
        assert_eq!(
            st.server_aliases_by_key_type.get("RSA").map(Vec::as_slice),
            Some(&["key".to_string()][..])
        );

        // With BOTH in one store the properly-marked one must come first, so a
        // caller taking `.first()` still prefers it.
        let mut two = crate::keystore::LoadedKeyStore::default();
        for (alias, der) in [("aserver", &server_only), ("zboth", &both)] {
            two.entries.insert(
                alias.to_string(),
                crate::keystore::KeyStoreEntry {
                    alias: alias.to_string(),
                    creation_time_ms: 0,
                    kind: crate::keystore::EntryKind::PrivateKey {
                        key_der: vec![0x30, 0x00],
                        chain: vec![der.clone()],
                    },
                },
            );
        }
        let st2 = build_key_manager_state(crate::keystore::keystore_register(two));
        let clients = st2
            .client_aliases_by_key_type
            .get("RSA")
            .expect("both aliases are RSA");
        assert_eq!(
            clients.first().map(String::as_str),
            Some("zboth"),
            "the clientAuth-marked alias must be preferred: got {clients:?}"
        );
        assert_eq!(clients.len(), 2, "neither alias may be dropped: {clients:?}");
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
    fn validate_chain_accepts_an_expired_certificate_that_is_the_anchor_itself() {
        // netty's `testMutualAuthDiffCerts` shape: a self-signed certificate
        // that expired long ago, installed as the ONLY trust anchor and then
        // presented by the peer as its own identity. RFC 5280 §6.1 validates
        // the certificates on the path against the anchor; the anchor is not
        // on the path, so its validity period is not one of the inputs. JSSE
        // completes this handshake.
        let expired_anchor = mk_cert(&CertSpec {
            not_before_utc: "990101000000Z",
            not_after_utc: "000101000000Z", // expired in 2000
            subject_cn: "self-signed-and-trusted",
            issuer_cn: "self-signed-and-trusted",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, expired_anchor.clone());
        validate_chain(&[expired_anchor.clone()], &trust)
            .expect("a presented certificate that IS the anchor is not path-validated");

        // The exemption is by full DER, not by subject: a DIFFERENT expired
        // certificate with the same subject is still expired.
        let impostor = mk_cert(&CertSpec {
            not_before_utc: "990101000000Z",
            not_after_utc: "000101000000Z",
            subject_cn: "self-signed-and-trusted",
            issuer_cn: "self-signed-and-trusted",
            spki_alg: OID_EC, // different SPKI => different DER
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
            subject_alt_dns: &[],
        });
        match validate_chain(&[impostor], &trust) {
            Err(TrustError::Expired { .. }) => {}
            other => panic!("a same-subject impostor must still be Expired, got {:?}", other),
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
    fn build_trust_manager_state_custom_store_is_restrictive() {
        let custom_anchor = mk_cert(&CertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "Custom Only Root",
            issuer_cn: "Custom Only Root",
            spki_alg: OID_RSA,
            key_usage_bits: Some(KU_KEY_CERT_SIGN),
            ext_key_usages: &[],
            basic_constraints_ca: Some(true),
            subject_alt_dns: &[],
        });
        let store = keystore::LoadedKeyStore {
            entries: [(
                "custom-root".to_string(),
                keystore::KeyStoreEntry {
                    alias: "custom-root".to_string(),
                    creation_time_ms: 0,
                    kind: keystore::EntryKind::TrustedCert {
                        cert_der: custom_anchor.clone(),
                    },
                },
            )]
            .into_iter()
            .collect(),
        };
        let id = keystore::keystore_register(store);
        let state = build_trust_manager_state(id);
        assert_eq!(state.keystore_id, id);
        assert_eq!(state.anchor_ders, vec![custom_anchor]);
        assert_eq!(state.anchors.len(), 1);
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
        build_tbs_with_alg(spec, &sig_alg_seq(spec.sig_alg_oid))
    }

    /// `build_tbs` with the whole `AlgorithmIdentifier` supplied, for the
    /// algorithms whose parameters are not ASN.1 NULL (RSASSA-PSS).
    fn build_tbs_with_alg(spec: &SignedCertSpec, sig_alg_der: &[u8]) -> Vec<u8> {
        let mut tbs: Vec<u8> = Vec::new();
        // version [0] EXPLICIT INTEGER 2 (v3)
        tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
        // serial
        tbs.extend_from_slice(&der_int(1));
        // signature alg (this MUST match the outer sigAlg byte-for-byte;
        // RFC 5280 §4.1.1.2 requires it)
        let sig_alg_seq = sig_alg_der.to_vec();
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
        assemble_cert_with_alg(tbs, &sig_alg_seq(sig_alg_oid), signature)
    }

    fn assemble_cert_with_alg(tbs: &[u8], sig_alg_der: &[u8], signature: &[u8]) -> Vec<u8> {
        let outer_sig_alg = sig_alg_der.to_vec();
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

    /// A CROSS-SIGNED intermediate: the same subject and the same key
    /// certified by two different roots, both supplied so that either root can
    /// be the trust anchor. Only one of them is on the path to the configured
    /// anchor; the other is an alternative, not a break — and this was refused
    /// as `BrokenChain` until path building landed.
    /// `io.netty.pkitesting.CertificateBuilderTest
    /// .authenticatingCrossSignedCertificate` is the real-world case.
    #[test]
    fn validate_chain_builds_a_path_past_a_cross_signed_intermediate() {
        let (trusted_pk, trusted_sk) = shared_rsa_root();
        let trusted_spki = Rsa::public_key_to_der(trusted_pk);
        let (other_pk, other_sk) = Rsa::generate_keypair(1024);
        let other_spki = Rsa::public_key_to_der(&other_pk);
        let (issuer_pk, issuer_sk) = Rsa::generate_keypair(1024);
        let issuer_spki = Rsa::public_key_to_der(&issuer_pk);

        let ca = |subject: &'static str, issuer: &'static str, spki: &[u8], sk: &RsaPrivateKey| {
            mk_rsa_signed_cert(
                &SignedCertSpec {
                    not_before_utc: "200101000000Z",
                    not_after_utc: "300101000000Z",
                    subject_cn: subject,
                    issuer_cn: issuer,
                    spki_der: spki,
                    sig_alg_oid: OID_SIG_SHA256_RSA,
                    key_usage_bits: Some(KU_KEY_CERT_SIGN),
                    ext_key_usages: &[],
                    basic_constraints_ca: Some(true),
                },
                sk,
            )
        };

        let trusted_root = ca("Trusted Root", "Trusted Root", &trusted_spki, trusted_sk);
        // Same subject, same key, two different issuing roots.
        let issuer_by_trusted = ca("Cross Issuer", "Trusted Root", &issuer_spki, trusted_sk);
        let issuer_by_other = ca("Cross Issuer", "Other Root", &issuer_spki, &other_sk);
        let leaf = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "leaf.example.com",
                issuer_cn: "Cross Issuer",
                spki_der: &other_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            &issuer_sk,
        );

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, trusted_root);

        // Presented with the intermediate that does NOT lead to the anchor
        // first — the order netty's cross-signing test produces.
        let r = validate_chain(
            &[
                leaf.clone(),
                issuer_by_other.clone(),
                issuer_by_trusted.clone(),
            ],
            &trust,
        );
        assert!(r.is_ok(), "cross-signed path must validate, got {r:?}");

        // Already a valid path: must still validate, by the untouched
        // presented-order route.
        let r = validate_chain(&[leaf.clone(), issuer_by_trusted, issuer_by_other.clone()], &trust);
        assert!(r.is_ok(), "already-ordered path must still validate, got {r:?}");

        // Path building must NOT rescue a set with no path to the anchor:
        // dropping the untrusted intermediate leaves a leaf with no issuer.
        let r = validate_chain(&[leaf, issuer_by_other], &trust);
        assert!(r.is_err(), "no path to the anchor must stay rejected, got {r:?}");
    }

    #[test]
    fn validate_chain_rejects_same_subject_fake_root() {
        let (trusted_pk, trusted_sk) = shared_rsa_root();
        let trusted_spki = Rsa::public_key_to_der(trusted_pk);
        let (fake_pk, fake_sk) = Rsa::generate_keypair(1024);
        let fake_spki = Rsa::public_key_to_der(&fake_pk);

        let trusted_root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Collision Root",
                issuer_cn: "Collision Root",
                spki_der: &trusted_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            trusted_sk,
        );
        let fake_root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Collision Root",
                issuer_cn: "Collision Root",
                spki_der: &fake_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            &fake_sk,
        );
        let leaf = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "leaf.example.com",
                issuer_cn: "Collision Root",
                spki_der: &fake_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            &fake_sk,
        );

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, trusted_root);
        match validate_chain(&[leaf, fake_root], &trust) {
            Err(TrustError::BadSignature { at }) => assert_eq!(at, 1),
            other => panic!("expected fake root signature failure, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_accepts_actual_anchor_when_subject_collides() {
        let (trusted_pk, trusted_sk) = shared_rsa_root();
        let trusted_spki = Rsa::public_key_to_der(trusted_pk);
        let (other_pk, other_sk) = Rsa::generate_keypair(1024);
        let other_spki = Rsa::public_key_to_der(&other_pk);

        let trusted_root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Collision Root",
                issuer_cn: "Collision Root",
                spki_der: &trusted_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            trusted_sk,
        );
        let other_root = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "490101000000Z",
                subject_cn: "Collision Root",
                issuer_cn: "Collision Root",
                spki_der: &other_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_KEY_CERT_SIGN | KU_DIGITAL_SIGNATURE),
                ext_key_usages: &[],
                basic_constraints_ca: Some(true),
            },
            &other_sk,
        );
        let leaf = mk_rsa_signed_cert(
            &SignedCertSpec {
                not_before_utc: "200101000000Z",
                not_after_utc: "300101000000Z",
                subject_cn: "leaf.example.com",
                issuer_cn: "Collision Root",
                spki_der: &trusted_spki,
                sig_alg_oid: OID_SIG_SHA256_RSA,
                key_usage_bits: Some(KU_DIGITAL_SIGNATURE | KU_KEY_ENCIPHERMENT),
                ext_key_usages: &[OID_KP_SERVER_AUTH],
                basic_constraints_ca: Some(false),
            },
            trusted_sk,
        );

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, trusted_root.clone());
        insert_anchor(&mut trust, other_root);
        let subject = parse_certificate(&trusted_root).unwrap().subject_der;
        assert_eq!(
            trust.anchors.get(&subject).map(|anchors| anchors.len()),
            Some(2)
        );

        validate_chain(&[leaf, trusted_root], &trust)
            .expect("actual stored anchor must not be hidden by same-subject roots");
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
        // Build a chain where the leaf claims id-Ed25519 — an OID the
        // verifier knows by name but does not implement (no EdDSA primitive
        // at this layer). The anchor is RSA-SHA256-signed so the chain
        // reaches Step 6 cleanly; the rejection comes from the leaf's OID
        // dispatch, and must be structural (NotImplemented) rather than
        // cryptographic, so callers can decide to delegate instead.
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

        // The *signature bytes* don't matter; we only need the outer sigAlg
        // OID to route to NotImplemented before the cryptographic verifier.
        let tbs = build_tbs(&SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "ed25519-leaf",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_ED25519,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        });
        let fake_sig = Rsa::sign_sha256(root_sk, &tbs);
        let leaf = assemble_cert(&tbs, OID_SIG_ED25519, &fake_sig);

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        match validate_chain(&[leaf, root], &trust) {
            Err(TrustError::NotImplemented { at, oid }) => {
                assert_eq!(at, 0);
                assert_eq!(oid, OID_SIG_ED25519.to_vec());
            }
            other => panic!("expected NotImplemented for Ed25519, got {:?}", other),
        }
    }

    /// `AlgorithmIdentifier` for RSASSA-PSS with SHA-256, MGF1-SHA-256 and an
    /// explicit salt length — the shape netty's `rsapss-ca-cert.cert` and the
    /// two `rsaValidation*.p12` fixtures carry.
    fn pss_sha256_alg_id(salt_len: u8) -> Vec<u8> {
        const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
        let sha256 = der_seq([der_oid(OID_SHA256), der_tlv(TAG_NULL, &[])].concat());
        let mgf = der_seq([der_oid(OID_MGF1), sha256.clone()].concat());
        let params = der_seq(
            [
                der_tlv(0xa0, &sha256),
                der_tlv(0xa1, &mgf),
                der_tlv(0xa2, &der_int(salt_len)),
            ]
            .concat(),
        );
        der_seq([der_oid(OID_SIG_RSA_PSS), params].concat())
    }

    #[test]
    fn validate_chain_rsa_pss_uses_the_params_salt_length_not_hlen() {
        use crate::crypto_impl::{rsa_sign_pss_ex, PssHash};

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

        let leaf_spec = SignedCertSpec {
            not_before_utc: "200101000000Z",
            not_after_utc: "300101000000Z",
            subject_cn: "pss-leaf",
            issuer_cn: "Real RSA Root",
            spki_der: &root_spki,
            sig_alg_oid: OID_SIG_RSA_PSS,
            key_usage_bits: Some(KU_DIGITAL_SIGNATURE),
            ext_key_usages: &[OID_KP_SERVER_AUTH],
            basic_constraints_ca: Some(false),
        };
        // saltLength 20 with a SHA-256 digest: RFC 4055's default, and what
        // the netty fixtures use. The salt length is NOT hLen.
        let alg = pss_sha256_alg_id(20);
        let tbs = build_tbs_with_alg(&leaf_spec, &alg);

        let sig20 = rsa_sign_pss_ex(root_sk, PssHash::Sha256, PssHash::Sha256, 20, &tbs);
        let leaf = assemble_cert_with_alg(&tbs, &alg, &sig20);

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        validate_chain(&[leaf, root.clone()], &trust)
            .expect("PSS chain with saltLength=20 must validate");

        // The regression this pins: a verifier that assumes salt == hLen
        // accepts THIS signature and rejects the one above. Both directions
        // are checked, so neither assumption can be reintroduced silently.
        let sig32 = rsa_sign_pss_ex(root_sk, PssHash::Sha256, PssHash::Sha256, 32, &tbs);
        let leaf_wrong_salt = assemble_cert_with_alg(&tbs, &alg, &sig32);
        match validate_chain(&[leaf_wrong_salt, root], &trust) {
            Err(TrustError::BadSignature { at }) => assert_eq!(at, 0),
            other => panic!("saltLength mismatch must be BadSignature, got {:?}", other),
        }
    }

    #[test]
    fn parse_rsa_pss_params_defaults_and_explicit_fields() {
        use crate::crypto_impl::PssHash;

        // Absent parameters => RFC 4055 defaults, which are NOT internally
        // consistent: SHA-1 with a 20-byte salt.
        let d = parse_rsa_pss_params(&[]).expect("absent params are the defaults");
        assert_eq!(d.hash, PssHash::Sha1);
        assert_eq!(d.mgf_hash, PssHash::Sha1);
        assert_eq!(d.salt_len, 20);
        // An explicit NULL means the same thing.
        assert_eq!(parse_rsa_pss_params(&der_tlv(TAG_NULL, &[])), Some(d));

        // The netty shape: SHA-256 / MGF1-SHA-256 / salt 20.
        let alg = pss_sha256_alg_id(20);
        let inner = read_tlv_tagged(&alg, TAG_SEQUENCE).unwrap();
        let oid = read_tlv_tagged(inner.content, TAG_OID).unwrap();
        let p = parse_rsa_pss_params(oid.rest).expect("netty-shaped params parse");
        assert_eq!(p.hash, PssHash::Sha256);
        assert_eq!(p.mgf_hash, PssHash::Sha256);
        assert_eq!(p.salt_len, 20);

        // A digest we do not implement is a refusal, not a silent default.
        const OID_SHA3_256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x08];
        let bad = der_seq(der_tlv(
            0xa0,
            &der_seq([der_oid(OID_SHA3_256), der_tlv(TAG_NULL, &[])].concat()),
        ));
        assert_eq!(parse_rsa_pss_params(&bad), None);
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

    // ====================================================================
    // RFC 5280 §4.2.1.10 name-constraints tests.
    // ====================================================================

    /// Build a `Name` SEQUENCE with one CN-RDN per supplied label, in order.
    fn name_with_rdns(cns: &[&str]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        for cn in cns {
            let cn_oid = der_oid(&[0x55, 0x04, 0x03]);
            let cn_val = der_tlv(0x13, cn.as_bytes());
            let atv = der_seq([cn_oid, cn_val].concat());
            body.extend_from_slice(&der_set(atv));
        }
        der_seq(body)
    }

    /// Encode a `NameConstraints` extension (critical) from `(GeneralName tag,
    /// base bytes)` entries. `der_tlv(tag, base)` is the GeneralName; each is
    /// wrapped in a one-element `GeneralSubtree` SEQUENCE.
    fn nc_extension(permitted: &[(u8, &[u8])], excluded: &[(u8, &[u8])]) -> Vec<u8> {
        fn subtrees(entries: &[(u8, &[u8])]) -> Vec<u8> {
            let mut body = Vec::new();
            for (tag, base) in entries {
                let gn = der_tlv(*tag, base);
                body.extend_from_slice(&der_seq(gn)); // GeneralSubtree ::= SEQ { base }
            }
            body
        }
        let mut seq_body = Vec::new();
        if !permitted.is_empty() {
            seq_body.extend_from_slice(&der_tlv(0xa0, &subtrees(permitted))); // [0]
        }
        if !excluded.is_empty() {
            seq_body.extend_from_slice(&der_tlv(0xa1, &subtrees(excluded))); // [1]
        }
        extension(OID_EXT_NAME_CONSTRAINTS, true, der_seq(seq_body))
    }

    /// Build + RSA-sign a v3 cert carrying BasicConstraints plus an arbitrary
    /// set of pre-encoded extensions. Keeps the name-constraints tests from
    /// having to widen the shared `SignedCertSpec`.
    fn mk_rsa_cert_with_exts(
        subject_cn: &str,
        issuer_cn: &str,
        spki_der: &[u8],
        is_ca: bool,
        extra_exts: &[u8],
        issuer_sk: &RsaPrivateKey,
    ) -> Vec<u8> {
        let mut tbs: Vec<u8> = Vec::new();
        tbs.extend_from_slice(&der_context_explicit(0, &der_int(2)));
        tbs.extend_from_slice(&der_int(1));
        tbs.extend_from_slice(&sig_alg_seq(OID_SIG_SHA256_RSA));
        tbs.extend_from_slice(&name_with_cn(issuer_cn));
        tbs.extend_from_slice(&der_seq(
            [der_utctime("200101000000Z"), der_utctime("490101000000Z")].concat(),
        ));
        tbs.extend_from_slice(&name_with_cn(subject_cn));
        tbs.extend_from_slice(spki_der);
        let mut exts: Vec<u8> = Vec::new();
        exts.extend_from_slice(&extension(OID_EXT_BASIC_CONSTRAINTS, true, bc_seq(is_ca)));
        exts.extend_from_slice(extra_exts);
        tbs.extend_from_slice(&der_context_explicit(3, &der_seq(exts)));
        let tbs = der_seq(tbs);
        let sig = Rsa::sign_sha256(issuer_sk, &tbs);
        assemble_cert(&tbs, OID_SIG_SHA256_RSA, &sig)
    }

    #[test]
    fn dns_constraint_matching_follows_rfc5280() {
        // Bare constraint: matches itself and any subdomain, label-aligned.
        assert!(dns_constraint_matches("example.com", "example.com"));
        assert!(dns_constraint_matches("example.com", "www.example.com"));
        assert!(dns_constraint_matches("example.com", "a.b.example.com"));
        assert!(!dns_constraint_matches("example.com", "notexample.com"));
        assert!(!dns_constraint_matches(
            "example.com",
            "example.com.evil.com"
        ));
        assert!(!dns_constraint_matches("example.com", "com"));
        // Empty constraint matches everything.
        assert!(dns_constraint_matches("", "anything.test"));
        // Leading-dot constraint: strict subdomains only.
        assert!(dns_constraint_matches(".example.com", "www.example.com"));
        assert!(!dns_constraint_matches(".example.com", "example.com"));
    }

    #[test]
    fn email_constraint_matching_follows_rfc5280() {
        // Full mailbox: exact match.
        assert!(email_constraint_matches(
            "ann@example.com",
            "ann@example.com"
        ));
        assert!(!email_constraint_matches(
            "ann@example.com",
            "bob@example.com"
        ));
        // Host constraint: any mailbox at that host, not a subdomain.
        assert!(email_constraint_matches("example.com", "ann@example.com"));
        assert!(!email_constraint_matches(
            "example.com",
            "ann@sub.example.com"
        ));
        // Leading-dot: subdomains only.
        assert!(email_constraint_matches(
            ".example.com",
            "ann@sub.example.com"
        ));
        assert!(!email_constraint_matches(".example.com", "ann@example.com"));
        // Malformed presented address (no host).
        assert!(!email_constraint_matches("example.com", "no-at-sign"));
    }

    #[test]
    fn ip_constraint_matching_uses_cidr_mask() {
        // 192.168.0.0/16 = addr 192.168.0.0, mask 255.255.0.0.
        let v4 = [192u8, 168, 0, 0, 255, 255, 0, 0];
        assert!(ip_constraint_matches(&v4, &[192, 168, 1, 5]));
        assert!(ip_constraint_matches(&v4, &[192, 168, 255, 255]));
        assert!(!ip_constraint_matches(&v4, &[10, 0, 0, 1]));
        assert!(!ip_constraint_matches(&v4, &[192, 169, 0, 1]));
        // Family mismatch: v4 constraint vs 16-byte address.
        assert!(!ip_constraint_matches(&v4, &[0u8; 16]));
        // IPv6 ::/0 (all-zero mask) matches anything v6.
        let v6_any = [0u8; 32];
        assert!(ip_constraint_matches(&v6_any, &[1u8; 16]));
    }

    #[test]
    fn uri_host_extraction() {
        assert_eq!(
            uri_host("https://host.example.com/path"),
            Some("host.example.com".into())
        );
        assert_eq!(
            uri_host("http://user@h.example.com:8443/x"),
            Some("h.example.com".into())
        );
        assert_eq!(
            uri_host("https://[2001:db8::1]:443/"),
            Some("2001:db8::1".into())
        );
        assert_eq!(
            uri_host("HTTPS://Host.Example.COM"),
            Some("host.example.com".into())
        );
        assert_eq!(uri_host(""), None);
    }

    #[test]
    fn dir_name_prefix_matching() {
        let base = name_with_rdns(&["Acme"]);
        let leaf = name_with_rdns(&["Acme", "leaf"]);
        let other = name_with_rdns(&["Other"]);
        // Constraint is an initial RDN prefix of the presented DN.
        assert!(dir_name_within(&base, &leaf));
        assert!(dir_name_within(&base, &base));
        // Different first RDN → not within.
        assert!(!dir_name_within(&other, &leaf));
        // Constraint longer than presented → not within.
        assert!(!dir_name_within(&leaf, &base));
    }

    #[test]
    fn parse_name_constraints_round_trips() {
        let ext = nc_extension(
            &[(SAN_TAG_DNS_NAME, b"example.com")],
            &[(SAN_TAG_DNS_NAME, b"bad.example.com")],
        );
        // `ext` is a full Extension SEQUENCE; pull the OCTET STRING value out.
        // Extension ::= SEQ { OID, BOOL critical, OCTET STRING value }.
        let seq = read_tlv_tagged(&ext, TAG_SEQUENCE).unwrap();
        let oid = read_tlv_tagged(seq.content, TAG_OID).unwrap();
        let after_oid = oid.rest;
        let crit = read_tlv(after_oid).unwrap();
        let octet = read_tlv_tagged(crit.rest, TAG_OCTET_STRING).unwrap();
        let nc = parse_name_constraints(octet.content).expect("parse NC");
        assert_eq!(nc.permitted.dns, vec!["example.com".to_string()]);
        assert_eq!(nc.excluded.dns, vec!["bad.example.com".to_string()]);
    }

    #[test]
    fn validate_chain_enforces_permitted_dns_subtree() {
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);

        // Root permits only the example.com dNSName subtree.
        let nc = nc_extension(&[(SAN_TAG_DNS_NAME, b"example.com")], &[]);
        let root = mk_rsa_cert_with_exts("NC Root", "NC Root", &root_spki, true, &nc, root_sk);

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());

        // Leaf inside the permitted subtree validates.
        let san_ok = extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(&["host.example.com"]),
        );
        let leaf_ok = mk_rsa_cert_with_exts(
            "host.example.com",
            "NC Root",
            &root_spki,
            false,
            &san_ok,
            root_sk,
        );
        validate_chain(&[leaf_ok, root.clone()], &trust)
            .expect("leaf within permitted subtree must validate");

        // Leaf outside the permitted subtree is rejected at index 0.
        let san_bad = extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(&["host.evil.com"]),
        );
        let leaf_bad = mk_rsa_cert_with_exts(
            "host.evil.com",
            "NC Root",
            &root_spki,
            false,
            &san_bad,
            root_sk,
        );
        match validate_chain(&[leaf_bad, root.clone()], &trust) {
            Err(TrustError::NameConstraintViolation { at, .. }) => assert_eq!(at, 0),
            other => panic!("expected NameConstraintViolation, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_enforces_excluded_dns_subtree() {
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);

        // Root excludes the evil.example.com subtree (permits everything else).
        let nc = nc_extension(&[], &[(SAN_TAG_DNS_NAME, b"evil.example.com")]);
        let root = mk_rsa_cert_with_exts("X Root", "X Root", &root_spki, true, &nc, root_sk);

        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());

        // A name outside the excluded subtree is fine.
        let san_ok = extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(&["host.good.com"]),
        );
        let leaf_ok = mk_rsa_cert_with_exts(
            "host.good.com",
            "X Root",
            &root_spki,
            false,
            &san_ok,
            root_sk,
        );
        validate_chain(&[leaf_ok, root.clone()], &trust).expect("non-excluded leaf must validate");

        // A name inside the excluded subtree is rejected.
        let san_bad = extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(&["www.evil.example.com"]),
        );
        let leaf_bad = mk_rsa_cert_with_exts(
            "www.evil.example.com",
            "X Root",
            &root_spki,
            false,
            &san_bad,
            root_sk,
        );
        match validate_chain(&[leaf_bad, root], &trust) {
            Err(TrustError::NameConstraintViolation { .. }) => {}
            other => panic!("expected NameConstraintViolation, got {:?}", other),
        }
    }

    #[test]
    fn validate_chain_without_name_constraints_is_unaffected() {
        // A chain whose CA carries NO NameConstraints extension must still
        // validate names of every kind (regression guard for the new step).
        let (root_pk, root_sk) = shared_rsa_root();
        let root_spki = Rsa::public_key_to_der(root_pk);
        let root =
            mk_rsa_cert_with_exts("Plain Root", "Plain Root", &root_spki, true, &[], root_sk);
        let san = extension(
            OID_EXT_SUBJECT_ALT_NAME,
            false,
            san_dns_value(&["anything.example"]),
        );
        let leaf = mk_rsa_cert_with_exts(
            "anything.example",
            "Plain Root",
            &root_spki,
            false,
            &san,
            root_sk,
        );
        let mut trust = TrustManagerState::default();
        insert_anchor(&mut trust, root.clone());
        validate_chain(&[leaf, root], &trust).expect("unconstrained chain must validate");
    }

    // -------------------------------------------------------------------
    // OCSP (RFC 6960) — DER encode/decode round-trip and signature check.
    // Network I/O (`ocsp_http_post`/`check_ocsp`) is exercised by the real
    // Tomcat OCSP suites (TestSecurity2017Ocsp, TestOcspEnabled,
    // TestOcspSoftFail*) rather than here; these tests cover the pure
    // ASN.1 + crypto primitives in isolation.
    // -------------------------------------------------------------------

    fn sample_cert_id() -> CertId {
        CertId {
            issuer_name_hash: [0x11; 20],
            issuer_key_hash: [0x22; 20],
            serial_der: vec![0x10, 0x03],
        }
    }

    #[test]
    fn build_ocsp_request_is_well_formed_der() {
        let req = build_ocsp_request(&sample_cert_id());
        // OCSPRequest ::= SEQUENCE { tbsRequest TBSRequest }
        let outer = read_tlv_tagged(&req, TAG_SEQUENCE).expect("outer SEQUENCE");
        let tbs = read_tlv_tagged(outer.content, TAG_SEQUENCE).expect("tbsRequest SEQUENCE");
        let request_list =
            read_tlv_tagged(tbs.content, TAG_SEQUENCE).expect("requestList SEQUENCE");
        let request =
            read_tlv_tagged(request_list.content, TAG_SEQUENCE).expect("Request SEQUENCE");
        let cert_id = read_tlv_tagged(request.content, TAG_SEQUENCE).expect("CertID SEQUENCE");
        let hash_alg =
            read_tlv_tagged(cert_id.content, TAG_SEQUENCE).expect("hashAlgorithm SEQUENCE");
        let oid = read_tlv_tagged(hash_alg.content, TAG_OID).expect("hashAlgorithm OID");
        assert_eq!(
            oid.content, OID_SHA1,
            "CertID must use SHA-1 per RFC 6960 convention"
        );
        // `oid.rest` is the hashAlgorithm's NULL parameter, still inside the
        // hashAlgorithm SEQUENCE; issuerNameHash is `hash_alg.rest` (CertID's
        // next sibling field, after the whole hashAlgorithm SEQUENCE).
        let name_hash = read_tlv_tagged(hash_alg.rest, TAG_OCTET_STRING).expect("issuerNameHash");
        assert_eq!(name_hash.content, &[0x11; 20]);
        let key_hash = read_tlv_tagged(name_hash.rest, TAG_OCTET_STRING).expect("issuerKeyHash");
        assert_eq!(key_hash.content, &[0x22; 20]);
        let serial = read_tlv_tagged(key_hash.rest, TAG_INTEGER).expect("serialNumber");
        assert_eq!(serial.content, &[0x10, 0x03]);
    }

    #[test]
    fn sha1_matches_known_vector() {
        // RFC 3174 test vector: SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89
        let digest = sha1(b"abc");
        assert_eq!(
            digest,
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }

    /// Build a minimal, well-formed `BasicOCSPResponse` DER wrapped in an
    /// `OCSPResponse` (`responseStatus = successful`), signed directly by
    /// `issuer_sk` (no delegated responder cert) over a single
    /// `SingleResponse` for `serial_der` with the given `status_tag`.
    fn build_test_ocsp_response(
        issuer_sk: &RsaPrivateKey,
        serial_der: &[u8],
        status_tag: u8,
    ) -> Vec<u8> {
        // responderID: byName [1] EXPLICIT Name -- content doesn't matter for
        // these tests, use an empty SEQUENCE (valid, if unusual, RDNSequence).
        let responder_id = der_tlv_encode(0xa1, &der_seq_encode(Vec::new()));
        let produced_at = der_tlv_encode(TAG_GENERALIZED_TIME, b"20200101000000Z");

        let cert_id = der_seq_encode({
            let mut h = der_seq_encode({
                let mut hh = der_oid_encode(OID_SHA1);
                hh.extend_from_slice(&der_null_encode());
                hh
            });
            h.extend_from_slice(&der_octet_encode(&[0x11; 20]));
            h.extend_from_slice(&der_octet_encode(&[0x22; 20]));
            h.extend_from_slice(&der_integer_from_content(serial_der));
            h
        });
        // certStatus: good=0x80 (primitive NULL), revoked=0xa1 (constructed,
        // needs a RevokedInfo body), unknown=0x82 (primitive NULL).
        let cert_status = match status_tag {
            0x80 => vec![0x80, 0x00],
            TAG_CONTEXT_1_CONSTRUCTED => {
                // RevokedInfo ::= SEQUENCE { revocationTime GeneralizedTime }
                der_tlv_encode(
                    TAG_CONTEXT_1_CONSTRUCTED,
                    &der_tlv_encode(TAG_GENERALIZED_TIME, b"20200101000000Z"),
                )
            }
            TAG_CONTEXT_2_PRIMITIVE => vec![0x82, 0x00],
            _ => panic!("unsupported status_tag in test helper"),
        };
        // SingleResponse ::= SEQUENCE { certID, certStatus, thisUpdate,
        //   [nextUpdate], [singleExtensions] } -- `produced_at`'s DER shape
        // (a GeneralizedTime TLV) is reused verbatim for thisUpdate.
        let single_response = der_seq_encode({
            let mut v = cert_id;
            v.extend_from_slice(&cert_status);
            v.extend_from_slice(&produced_at);
            v
        });

        let responses = der_seq_encode(single_response);
        let response_data = der_seq_encode({
            let mut v = responder_id;
            v.extend_from_slice(&produced_at);
            v.extend_from_slice(&responses);
            v
        });

        let sig_alg = der_seq_encode({
            let mut v = der_oid_encode(OID_SIG_SHA256_RSA);
            v.extend_from_slice(&der_null_encode());
            v
        });
        let signature = Rsa::sign_sha256(issuer_sk, &response_data);
        let sig_bitstring = der_tlv_encode(TAG_BIT_STRING, &{
            let mut v = vec![0u8];
            v.extend_from_slice(&signature);
            v
        });

        let basic_response = der_seq_encode({
            let mut v = response_data;
            v.extend_from_slice(&sig_alg);
            v.extend_from_slice(&sig_bitstring);
            v
        });

        let response_bytes = der_seq_encode({
            let mut v = der_oid_encode(OID_OCSP_BASIC_RESPONSE);
            v.extend_from_slice(&der_octet_encode(&basic_response));
            v
        });

        der_seq_encode({
            let mut v = vec![TAG_ENUMERATED, 0x01, 0x00]; // responseStatus = successful(0)
            v.extend_from_slice(&der_tlv_encode(TAG_CONTEXT_0, &response_bytes));
            v
        })
    }

    #[test]
    fn ocsp_response_good_status_parses_and_verifies() {
        let (_pk, sk) = shared_rsa_root();
        let serial = vec![0x10, 0x03];
        let der = build_test_ocsp_response(sk, &serial, 0x80);
        let parsed = parse_ocsp_response(&der).expect("parse should succeed");
        assert_eq!(parsed.responses.len(), 1);
        assert_eq!(parsed.responses[0].status, OcspStatusTag::Good);
        assert_eq!(parsed.responses[0].cert_id_serial, serial);

        let root_spki = Rsa::public_key_to_der(&shared_rsa_root().0);
        verify_ocsp_response_signature(&parsed, &root_spki, None)
            .expect("signature must verify against the real issuer key");
    }

    #[test]
    fn ocsp_response_revoked_status_parses() {
        let (_pk, sk) = shared_rsa_root();
        let serial = vec![0x10, 0x03];
        let der = build_test_ocsp_response(sk, &serial, TAG_CONTEXT_1_CONSTRUCTED);
        let parsed = parse_ocsp_response(&der).expect("parse should succeed");
        assert_eq!(parsed.responses[0].status, OcspStatusTag::Revoked);
    }

    #[test]
    fn ocsp_response_unknown_status_parses() {
        let (_pk, sk) = shared_rsa_root();
        let serial = vec![0x10, 0x03];
        let der = build_test_ocsp_response(sk, &serial, TAG_CONTEXT_2_PRIMITIVE);
        let parsed = parse_ocsp_response(&der).expect("parse should succeed");
        assert_eq!(parsed.responses[0].status, OcspStatusTag::Unknown);
    }

    #[test]
    fn ocsp_response_signature_rejected_against_wrong_key() {
        let (_pk, sk) = shared_rsa_root();
        let serial = vec![0x10, 0x03];
        let der = build_test_ocsp_response(sk, &serial, 0x80);
        let parsed = parse_ocsp_response(&der).expect("parse should succeed");

        // A different keypair's SPKI must NOT validate this response's signature.
        let (other_pk, _other_sk) = Rsa::generate_keypair(1024);
        let wrong_spki = Rsa::public_key_to_der(&other_pk);
        let err = verify_ocsp_response_signature(&parsed, &wrong_spki, None)
            .expect_err("signature must NOT verify against an unrelated key");
        assert!(
            err.contains("does not verify"),
            "unexpected error text: {err}"
        );
    }

    #[test]
    fn ocsp_response_try_later_status_is_rejected() {
        // responseStatus = tryLater(3), no responseBytes at all — matches
        // TesterOcspResponderServlet's TRY_LATER fixed-response shape.
        let der = der_seq_encode(vec![TAG_ENUMERATED, 0x01, 0x03]);
        let err = parse_ocsp_response(&der).expect_err("tryLater must not parse as successful");
        assert!(err.contains("tryLater"), "unexpected error text: {err}");
    }
}
