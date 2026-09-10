// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP5.2 — `KeyStore` real PKCS#12 + JKS parse.
//!
//! Implements the `engine*` surface real-JDK exposes from
//! `sun/security/pkcs12/PKCS12KeyStore` and `sun/security/provider/JavaKeyStore`
//! (plus the `$JKS` and `$DualFormatJKS` inner-class aliases). Loads a
//! PKCS#12 PFX or a JKS keystore, decrypts shrouded key bags with the supplied
//! password, and exposes private keys + cert chains to the rest of the runtime
//! (TLS, X509KeyManager, X509TrustManager).
//!
//! ## Format detection
//!
//! - PKCS#12 always starts with ASN.1 `SEQUENCE` (`0x30 ..`). Delegates to the
//!   `p12` crate, which gives us `PFX::parse(bytes)` + `PFX::bags(password)`
//!   returning typed `SafeBag`s (cert vs shrouded-key vs other).
//! - JKS starts with the magic `0xFEEDFEED` (4 bytes BE). Hand-rolled walker
//!   (the format is fully open — see Wikipedia: JKS). Trailing 20-byte
//!   integrity tag is verified with the JKS-specific construction:
//!   `SHA1(password_utf16be || "Mighty Aphrodite" || body)` — note this is
//!   *not* a standard HMAC, and an empty password is encoded as the empty
//!   byte sequence (not `[0x00, 0x00]`).
//!
//! ## What we expose to the VM
//!
//! Each loaded store is assigned a 32-bit `store_id` and stashed in the
//! per-process `KEYSTORE_REGISTRY`. A synthetic Java mirror is allocated for
//! each entry the VM asks about:
//!
//! - `engineGetKey` returns a `java/security/PrivateKey` proxy whose 4 fields
//!   are `(algo_idx, key_size_bits, key_len_bytes, key_id)`. The TLS layer
//!   pulls the DER through `keystore_get_private_key(store_id, alias)` /
//!   `keystore_get_chain` (the public shim exported below).
//! - `engineGetCertificate` returns a `java/security/cert/X509Certificate`
//!   proxy with fields `(subject, issuer, cert_id)`. The X.509 parser at
//!   `crate::security_manager::x509` is *not* called from this module — it
//!   would create a back-edge with that crate. Instead we keep the DER and
//!   let consumers decode on demand.
//!
//! ## Tests
//!
//! Embedded `#[cfg(test)] mod tests` covers:
//! 1. Round-trip JKS load + alias enumeration on a known fixture.
//! 2. Round-trip PKCS#12 load + alias enumeration on a known fixture.
//! 3. Magic-byte format detection.
//! 4. JKS HMAC pass / mismatch on a corrupted byte.
//! 5. Wrong-password rejection on a shrouded PKCS#12 key bag.
//! 6. End-to-end `engineLoad` -> `engineAliases` -> `engineGetCertificate`
//!    on the JKS fixture through a `MockNativeContext`.
//!
//! Fixture bytes are inlined as `static [u8]` blobs (≤ 4 KiB each) so the
//! tests run hermetically with zero filesystem dependency.

#![allow(clippy::needless_range_loop)]

use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::OnceLock;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use parking_lot::RwLock;

use crate::crypto_impl;
use crate::try_alloc_concurrent_synthetic;

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// A single entry parsed from a keystore.
#[derive(Clone, Debug)]
pub struct KeyStoreEntry {
    /// Original alias as it appeared in the file. JKS lowercases at write
    /// time, PKCS#12 preserves whatever the producer wrote.
    pub alias: String,
    /// Creation time in milliseconds since epoch (JKS provides this; PKCS#12
    /// generally does not, in which case we report 0).
    pub creation_time_ms: i64,
    pub kind: EntryKind,
}

/// What flavor of entry this is. Keys are kept as raw DER (PKCS#8 `PrivateKey`
/// SEQUENCE for keys decrypted out of a shrouded bag, or whatever JKS stored
/// — JKS's "encrypted" format is a known-broken SHA-1 stream cipher, but the
/// decryption is still defined and we honour it).
#[derive(Clone, Debug)]
pub enum EntryKind {
    PrivateKey {
        /// Decrypted PKCS#8 `PrivateKey` DER (the bytes of the SEQUENCE,
        /// algorithm + private-key OCTET STRING).
        key_der: Vec<u8>,
        /// Cert chain in DER form, leaf first.
        chain: Vec<Vec<u8>>,
    },
    TrustedCert {
        /// X.509 cert DER.
        cert_der: Vec<u8>,
    },
    /// Symmetric key material from a PKCS#12 SecretBag, carried together with
    /// the JCA algorithm name it was stored under.
    ///
    /// The name is not decoration. `SunPKCS12` encodes a `SecretKeyEntry` as
    /// `SEQUENCE { INTEGER 0, AlgorithmId.get(key.getAlgorithm()), OCTET
    /// STRING key.getEncoded() }`, so the algorithm survives a round trip only
    /// as that OID. This module used to discard it and hand every recovered
    /// secret key back as algorithm `"RAW"` -- which is the key's FORMAT, not
    /// its algorithm -- so `Cipher.getInstance(key.getAlgorithm())` on a key
    /// read out of a keystore failed where HotSpot succeeded.
    SecretKey {
        key_bytes: Vec<u8>,
        algorithm: String,
    },
}

/// One loaded keystore. The map keys are case-preserved aliases.
///
/// `IndexMap`, NOT `HashMap` — real JDK's `JavaKeyStore`/`PKCS12KeyStore`
/// both use a `LinkedHashMap` internally, so `KeyStore.aliases()` enumerates
/// entries in the order they were read from the file/inserted, not an
/// arbitrary hash order. Spring Boot's `SslInfo`
/// (`SslInfoTests.trustStoreCertificatesShouldProvideSslInfo` et al.) asserts
/// on that exact positional order (`getTrustStoreCertificateChains().get(0)`
/// is the FIRST entry in the file, not the alphabetically-first one) —
/// `std::collections::HashMap`'s randomized iteration order can't satisfy
/// that.
#[derive(Clone, Debug, Default)]
pub struct LoadedKeyStore {
    pub entries: IndexMap<String, KeyStoreEntry>,
}

/// Errors produced by the keystore parsers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum KeyStoreError {
    #[error("not a recognised keystore (no PKCS#12 SEQUENCE prefix nor 0xFEEDFEED magic)")]
    UnknownFormat,
    #[error("keystore truncated at offset {0}")]
    Truncated(usize),
    #[error("JKS magic mismatch")]
    BadJksMagic,
    #[error("JKS unknown version {0}")]
    BadJksVersion(u32),
    #[error("JKS unknown entry tag {0}")]
    BadJksTag(u32),
    #[error("JKS HMAC integrity check failed")]
    JksMacMismatch,
    #[error("PKCS#12 parse failed: {0}")]
    Pkcs12Parse(String),
    #[error("PKCS#12 MAC verification failed (wrong password?)")]
    Pkcs12MacFailed,
    #[error("PKCS#12 shrouded key bag decrypt failed (wrong password?)")]
    Pkcs12KeyDecryptFailed,
    #[error("alias {0:?} not found")]
    UnknownAlias(String),
}

// ---------------------------------------------------------------------------
// Process-wide registry
// ---------------------------------------------------------------------------

static KEYSTORE_REGISTRY: OnceLock<RwLock<KeyStoreRegistry>> = OnceLock::new();

#[derive(Default)]
struct KeyStoreRegistry {
    next_id: i32,
    stores: HashMap<i32, LoadedKeyStore>,
}

fn registry() -> &'static RwLock<KeyStoreRegistry> {
    KEYSTORE_REGISTRY.get_or_init(|| {
        RwLock::new(KeyStoreRegistry {
            next_id: 1,
            stores: HashMap::new(),
        })
    })
}

/// Stash a parsed keystore and return its assigned id.
pub fn keystore_register(store: LoadedKeyStore) -> i32 {
    let mut g = registry().write();
    let id = g.next_id;
    g.next_id = g.next_id.checked_add(1).unwrap_or(1);
    g.stores.insert(id, store);
    id
}

/// Look up a parsed keystore by id.
pub fn keystore_lookup(id: i32) -> Option<LoadedKeyStore> {
    registry().read().stores.get(&id).cloned()
}

/// Insert/replace a trusted-cert entry in an already-registered store
/// (in-memory `KeyStore.setCertificateEntry`). Reads and writes share this
/// side-table, so the entry is visible to `engineAliases`/`engineSize`/
/// `engineGetCertificate`. Returns true if the store existed.
pub fn keystore_set_cert_entry(id: i32, alias: &str, cert_der: Vec<u8>) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: EntryKind::TrustedCert { cert_der },
            },
        );
        true
    } else {
        false
    }
}

/// Insert/replace a private-key entry in an already-registered store
/// (in-memory `KeyStore.setKeyEntry(String, Key, char[], Certificate[])`).
/// Companion to `keystore_set_cert_entry` for the PrivateKey case -- same
/// rationale: the real `PKCS12KeyStoreSpi.engineSetKeyEntry` bytecode (when
/// reached without a native override) mutates the real SPI object's own
/// `entries` field, invisible to CratonVM's side-table-backed reads
/// (`engineAliases`/`engineSize`/`keystore_get_private_key`) and, critically,
/// to `keystore_set_pending_km_identity` -- so `KeyManagerFactory.init`
/// found no identity to stage for the TLS layer. Returns true if the store
/// existed.
pub fn keystore_set_key_entry(id: i32, alias: &str, key_der: Vec<u8>, chain: Vec<Vec<u8>>) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: EntryKind::PrivateKey { key_der, chain },
            },
        );
        true
    } else {
        false
    }
}

/// Insert/replace a secret-key entry in an already-registered store
/// (`KeyStore.setEntry` with a `SecretKeyEntry`, or the pre-1.5
/// `setKeyEntry(alias, SecretKey, password, null)`).
///
/// Companion to `keystore_set_key_entry` for the symmetric case. Before it
/// existed, both routes either did nothing at all (`setEntry` -- see
/// `engine_set_entry`) or stored the raw bytes as a `PrivateKey` entry, so
/// `getKey` handed back a `PrivateKey` proxy claiming algorithm RSA for what
/// the caller had put in as an AES `SecretKeySpec`.
pub fn keystore_set_secret_key_entry(
    id: i32,
    alias: &str,
    key_bytes: Vec<u8>,
    algorithm: &str,
) -> bool {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        store.entries.insert(
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: EntryKind::SecretKey {
                    key_bytes,
                    algorithm: algorithm.to_string(),
                },
            },
        );
        true
    } else {
        false
    }
}

/// Remove an entry from a registered store (`KeyStore.deleteEntry`).
pub fn keystore_delete_entry(id: i32, alias: &str) {
    let mut g = registry().write();
    if let Some(store) = g.stores.get_mut(&id) {
        // `shift_remove`, not `swap_remove`: preserves the remaining
        // entries' relative order (matches a real `LinkedHashMap.remove`),
        // consistent with `LoadedKeyStore::entries`'s doc comment.
        store.entries.shift_remove(alias);
    }
}

/// Convenience for the TLS layer: fetch the PKCS#8 private-key DER for an
/// alias, plus the cert chain (leaf first), without exposing the registry
/// internals. Returns `None` if no `PrivateKey` entry exists.
pub fn keystore_get_private_key(id: i32, alias: &str) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let store = registry().read().stores.get(&id).cloned()?;
    let entry = store.entries.get(alias)?;
    if let EntryKind::PrivateKey { key_der, chain } = &entry.kind {
        Some((key_der.clone(), chain.clone()))
    } else {
        None
    }
}

/// Convenience for the TrustManager layer: fetch the cert DER for a trusted
/// cert entry, or the leaf cert for a private-key entry.
pub fn keystore_get_cert_der(id: i32, alias: &str) -> Option<Vec<u8>> {
    let store = registry().read().stores.get(&id).cloned()?;
    let entry = store.entries.get(alias)?;
    match &entry.kind {
        EntryKind::TrustedCert { cert_der } => Some(cert_der.clone()),
        EntryKind::PrivateKey { chain, .. } => chain.first().cloned(),
        EntryKind::SecretKey { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Format detection + dispatch
// ---------------------------------------------------------------------------

/// JKS magic constant `0xFEEDFEED` (file's first 4 bytes, big-endian).
pub const JKS_MAGIC: u32 = 0xFEEDFEED;

/// JCEKS magic constant `0xCECECECE`.
///
/// JCEKS is JKS's record layout under a different magic and with a stronger
/// key-protection PBE. This VM's JKS path does not decrypt private keys, so the
/// two are the same parser here and the magic is the whole difference --
/// MEASURED against HotSpot by `apps/probes/KeyStoreTypeProbe.java`, whose
/// `[JCEKS] stored magic` row is `cececece`.
pub const JCEKS_MAGIC: u32 = 0xCECE_CECE;

/// JKS HMAC salt. The construction is `SHA1(passwd_utf16be || SALT || body)`.
const JKS_HMAC_SALT: &[u8] = b"Mighty Aphrodite";

/// Detect format and load. `password` is the UTF-16-style password real-JDK
/// hands us as a `char[]`; both parsers receive the raw bytes the user typed
/// (UTF-8 of those chars) so they can apply their per-format mixing.
pub fn load_keystore(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    load_keystore_ex(bytes, password, true)
}

/// `verify_mac=false` mirrors real-JDK's `KeyStore.load(stream, null)`
/// contract: a Java `null` password (as opposed to an empty `char[]`)
/// disables PKCS#12 integrity checking entirely rather than checking against
/// an empty password. Callers that can't distinguish "no password supplied"
/// from "empty password supplied" should keep using [`load_keystore`].
pub(crate) fn load_keystore_ex(
    bytes: &[u8],
    password: &[u8],
    verify_mac: bool,
) -> Result<LoadedKeyStore, KeyStoreError> {
    if bytes.len() < 4 {
        return Err(KeyStoreError::Truncated(0));
    }
    let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic == JKS_MAGIC || magic == JCEKS_MAGIC {
        load_jks(bytes, password)
    } else if bytes[0] == 0x30 {
        load_pkcs12_ex(bytes, password, verify_mac)
    } else {
        Err(KeyStoreError::UnknownFormat)
    }
}

// ---------------------------------------------------------------------------
// PKCS#12 — backed by the `p12` crate
// ---------------------------------------------------------------------------

/// JCA secret-key algorithm name -> the OID `SunPKCS12` writes into a
/// SecretBag's inner `AlgorithmIdentifier`.
///
/// MEASURED on JDK 25, not guessed: this is exactly what
/// `AlgorithmId.get(name).getOID()` answers, and two of the rows are not the
/// OID an educated guess produces (`DESede` is the OIW `1.3.14.3.2.17`, not
/// PKCS#3's `des-EDE3-CBC`; `Blowfish` is `…3029.1.1.2`, not `…3029.1.2`).
/// Getting one wrong is not a parse error on either side — the key material
/// still round-trips — it just comes back out under a DIFFERENT algorithm
/// name, which is the quiet kind of wrong this whole record is about.
const SECRET_KEY_ALG_OIDS: &[(&str, &[u64])] = &[
    ("AES", &[2, 16, 840, 1, 101, 3, 4, 1]),
    ("DESede", &[1, 3, 14, 3, 2, 17]),
    ("DES", &[1, 3, 14, 3, 2, 7]),
    ("RC2", &[1, 2, 840, 113549, 3, 2]),
    ("ARCFOUR", &[1, 2, 840, 113549, 3, 4]),
    ("Blowfish", &[1, 3, 6, 1, 4, 1, 3029, 1, 1, 2]),
    ("HmacSHA1", &[1, 2, 840, 113549, 2, 7]),
    ("HmacSHA224", &[1, 2, 840, 113549, 2, 8]),
    ("HmacSHA256", &[1, 2, 840, 113549, 2, 9]),
    ("HmacSHA384", &[1, 2, 840, 113549, 2, 10]),
    ("HmacSHA512", &[1, 2, 840, 113549, 2, 11]),
];

/// OID -> the name to report on the way back OUT, which is what
/// `AlgorithmId.getName()` answers on JDK 25 — also measured.
///
/// This is deliberately NOT the inverse of [`SECRET_KEY_ALG_OIDS`]: the JDK is
/// itself asymmetric for three algorithms (`AlgorithmId.get("DES")` encodes
/// `1.3.14.3.2.7`, but reading that OID back names it `"DES/CBC"`). Matching
/// the JDK's answer beats internal tidiness here, because the thing that
/// compares the two is a differential against HotSpot. The `des-EDE3-CBC` row
/// has no write counterpart at all; it exists because OpenSSL-produced files
/// use it.
const SECRET_KEY_ALG_NAMES: &[(&[u64], &str)] = &[
    (&[2, 16, 840, 1, 101, 3, 4, 1], "AES"),
    (&[1, 3, 14, 3, 2, 17], "DESede"),
    (&[1, 2, 840, 113549, 3, 7], "DESede/CBC/NoPadding"),
    (&[1, 3, 14, 3, 2, 7], "DES/CBC"),
    (&[1, 2, 840, 113549, 3, 2], "RC2/CBC/PKCS5Padding"),
    (&[1, 2, 840, 113549, 3, 4], "ARCFOUR"),
    (&[1, 3, 6, 1, 4, 1, 3029, 1, 1, 2], "Blowfish"),
    (&[1, 2, 840, 113549, 2, 7], "HmacSHA1"),
    (&[1, 2, 840, 113549, 2, 8], "HmacSHA224"),
    (&[1, 2, 840, 113549, 2, 9], "HmacSHA256"),
    (&[1, 2, 840, 113549, 2, 10], "HmacSHA384"),
    (&[1, 2, 840, 113549, 2, 11], "HmacSHA512"),
];

/// Parse a dotted OID string (`"1.2.840.113549.3.7"`) into components.
///
/// Rejects anything that is not a well-formed OID, including the arc-0/1
/// constraint the DER encoder relies on — a bad first arc would panic the
/// writer rather than produce a wrong file, which is worse.
fn parse_dotted_oid(text: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = text.split('.').map(|c| c.parse::<u64>().ok()).collect();
    let parts = parts?;
    if parts.len() < 2 || parts[0] > 2 || (parts[0] < 2 && parts[1] >= 40) {
        return None;
    }
    Some(parts)
}

/// The OID to encode a JCA secret-key algorithm name under, or `None` when this
/// VM cannot encode it at all (which is also true of a real JDK — see
/// `AlgorithmId.get("ChaCha20")`, a `NoSuchAlgorithmException`).
///
/// Three name shapes resolve, in order: a name this VM writes
/// ([`SECRET_KEY_ALG_OIDS`]); a name this VM *reads* ([`SECRET_KEY_ALG_NAMES`]),
/// so `load` → `store` of a `"DES/CBC"` entry is lossless rather than fatal;
/// and a bare dotted OID, which is what [`secret_key_alg_name`] answers for an
/// OID neither table names — same reason.
fn secret_key_alg_oid(name: &str) -> Option<yasna::models::ObjectIdentifier> {
    if let Some((_, oid)) = SECRET_KEY_ALG_OIDS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        return Some(yasna::models::ObjectIdentifier::from_slice(oid));
    }
    if let Some((oid, _)) = SECRET_KEY_ALG_NAMES
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
    {
        return Some(yasna::models::ObjectIdentifier::from_slice(oid));
    }
    parse_dotted_oid(name).map(|parts| yasna::models::ObjectIdentifier::from_slice(&parts))
}

/// The JCA algorithm name for an OID read out of a SecretBag, falling back to
/// the dotted OID text (what `AlgorithmId.getName()` answers for an OID it has
/// no name for).
fn secret_key_alg_name(oid: &yasna::models::ObjectIdentifier) -> String {
    let components = oid.components().as_slice();
    for (known, name) in SECRET_KEY_ALG_NAMES {
        if components == *known {
            return (*name).to_string();
        }
    }
    components
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// [`secret_key_alg_name`] over a parsed `p12::AlgorithmIdentifier`.
///
/// `AlgorithmIdentifier::parse` folds three legacy PKCS#12 OIDs into named
/// variants before anything reaches `OtherAlg`. None of them is a secret-key
/// algorithm, but a malformed/hostile file could still put one there, so name
/// them explicitly instead of falling through to a wrong default.
fn secret_alg_name(alg: &p12::AlgorithmIdentifier) -> String {
    match alg {
        p12::AlgorithmIdentifier::OtherAlg(other) => secret_key_alg_name(&other.algorithm_type),
        p12::AlgorithmIdentifier::Sha1 => "SHA1".to_string(),
        p12::AlgorithmIdentifier::PbewithSHAAnd40BitRC2CBC(_) => "PBEWithSHA1AndRC2_40".to_string(),
        p12::AlgorithmIdentifier::PbeWithSHAAnd3KeyTripleDESCBC(_) => {
            "PBEWithSHA1AndDESede".to_string()
        }
    }
}

/// Crate-private `p12::bmp_string`, re-implemented: UTF-16BE + trailing 0x0000.
/// The PKCS#12 PBE/MAC password mixing operates on this BMPString form.
fn pkcs12_bmp_string(s: &str) -> Vec<u8> {
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2 + 2);
    for c in utf16 {
        bytes.push((c >> 8) as u8);
        bytes.push((c & 0xff) as u8);
    }
    bytes.push(0x00);
    bytes.push(0x00);
    bytes
}

/// The `p12` crate's built-in MAC verifier only implements SHA-1.  That was
/// correct for the crate's own legacy fixtures, but current SunPKCS12 emits a
/// SHA-256 `MacData` by default.  Keep the PKCS#12 KDF local so we can verify
/// the digest recorded by the file rather than silently treating every MAC as
/// SHA-1.
#[derive(Clone, Copy)]
enum Pkcs12MacDigest {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Pkcs12MacDigest {
    fn output_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha224 => 28,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    fn block_len(self) -> usize {
        match self {
            Self::Sha1 | Self::Sha224 | Self::Sha256 => 64,
            Self::Sha384 | Self::Sha512 => 128,
        }
    }

    fn hash(self, bytes: &[u8]) -> Vec<u8> {
        use sha1::Digest as _;

        match self {
            Self::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            Self::Sha224 => sha2::Sha224::digest(bytes).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(bytes).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
        }
    }
}

fn pkcs12_mac_digest(algorithm: &p12::AlgorithmIdentifier) -> Option<Pkcs12MacDigest> {
    use p12::AlgorithmIdentifier::{OtherAlg, Sha1};

    match algorithm {
        Sha1 => Some(Pkcs12MacDigest::Sha1),
        OtherAlg(other) => match other.algorithm_type.components().as_slice() {
            // NIST SHA-2 digest OIDs, as used by JDK 8u191+ SunPKCS12.
            [2, 16, 840, 1, 101, 3, 4, 2, 4] => Some(Pkcs12MacDigest::Sha224),
            [2, 16, 840, 1, 101, 3, 4, 2, 1] => Some(Pkcs12MacDigest::Sha256),
            [2, 16, 840, 1, 101, 3, 4, 2, 2] => Some(Pkcs12MacDigest::Sha384),
            [2, 16, 840, 1, 101, 3, 4, 2, 3] => Some(Pkcs12MacDigest::Sha512),
            _ => None,
        },
        _ => None,
    }
}

fn pkcs12_mac_kdf(
    digest: Pkcs12MacDigest,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    id: u8,
    output_len: usize,
) -> Vec<u8> {
    let v = digest.block_len();
    let u = digest.output_len();
    let repeat_to_block = |input: &[u8]| {
        if input.is_empty() {
            return Vec::new();
        }
        let len = v * input.len().div_ceil(v);
        input.iter().copied().cycle().take(len).collect::<Vec<_>>()
    };

    let mut i = repeat_to_block(salt);
    i.extend(repeat_to_block(password));
    let d = vec![id; v];
    let mut out = Vec::with_capacity(output_len);

    while out.len() < output_len {
        let mut round = Vec::with_capacity(d.len() + i.len());
        round.extend_from_slice(&d);
        round.extend_from_slice(&i);
        let mut a = digest.hash(&round);
        for _ in 1..iterations.max(1) {
            a = digest.hash(&a);
        }

        let b = a.iter().copied().cycle().take(v).collect::<Vec<_>>();
        for block in i.chunks_exact_mut(v) {
            let mut carry = 1u16;
            for (byte, addend) in block.iter_mut().rev().zip(b.iter().rev()) {
                let sum = *byte as u16 + *addend as u16 + carry;
                *byte = sum as u8;
                carry = sum >> 8;
            }
        }
        out.extend_from_slice(&a);
    }
    out.truncate(output_len);
    out
}

fn pkcs12_hmac(digest: Pkcs12MacDigest, key: &[u8], data: &[u8]) -> Vec<u8> {
    let block_len = digest.block_len();
    let mut padded_key = if key.len() > block_len {
        digest.hash(key)
    } else {
        key.to_vec()
    };
    padded_key.resize(block_len, 0);

    let mut inner = Vec::with_capacity(block_len + data.len());
    inner.extend(padded_key.iter().map(|byte| byte ^ 0x36));
    inner.extend_from_slice(data);
    let inner_hash = digest.hash(&inner);

    let mut outer = Vec::with_capacity(block_len + inner_hash.len());
    outer.extend(padded_key.iter().map(|byte| byte ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    digest.hash(&outer)
}

fn verify_pkcs12_mac(pfx: &p12::PFX, password: &str) -> bool {
    let Some(mac_data) = &pfx.mac_data else {
        return true;
    };
    let Some(digest) = pkcs12_mac_digest(&mac_data.mac.digest_algorithm) else {
        return false;
    };
    let password = pkcs12_bmp_string(password);
    let Some(auth_safe) = pfx.auth_safe.data(&password) else {
        return false;
    };
    let key = pkcs12_mac_kdf(
        digest,
        &password,
        &mac_data.salt,
        mac_data.iterations,
        3,
        digest.output_len(),
    );
    constant_time_eq(&pkcs12_hmac(digest, &key, &auth_safe), &mac_data.mac.digest)
}

/// BER-mode equivalent of `p12::PFX::bags`. Identical structure to the crate's
/// own `bags()` (auth_safe -> SEQUENCE OF ContentInfo -> per-content data ->
/// SEQUENCE OF SafeBag) but parsed with `yasna::parse_ber`, which does not
/// enforce DER canonical SET-OF ordering. The JDK emits trusted-cert bag
/// attribute sets out of DER order (friendlyName before the Oracle
/// trustedKeyUsage attribute); SunJSSE accepts them and so must we. BER is a
/// strict superset of DER — every field is still fully decoded and type-checked;
/// only the DER-only canonical-ordering constraint is relaxed (kcfull #12).
fn bags_ber(
    pfx: &p12::PFX,
    password_str: &str,
    tolerate_undecryptable: bool,
) -> Result<Vec<p12::SafeBag>, yasna::ASN1Error> {
    let password = pkcs12_bmp_string(password_str);
    let data = pfx
        .auth_safe
        .data(&password)
        .ok_or_else(|| yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid))?;
    let contents = yasna::parse_ber(&data, |r| r.collect_sequence_of(p12::ContentInfo::parse))?;
    let mut result = Vec::new();
    for content in contents.iter() {
        let inner = match content {
            p12::ContentInfo::Data(data) => Some(data.clone()),
            p12::ContentInfo::EncryptedData(encrypted) => encrypted
                .data(&password)
                // `p12` 0.6 only decrypts the legacy PKCS#12 PBE algorithms.
                // Current SunPKCS12 encrypts certificate SafeContents with
                // PBES2/PBKDF2/AES, so use the recorded PBES2 parameters when
                // the crate deliberately returns None for that newer form.
                .or_else(|| {
                    decrypt_pbes2_content(
                        &encrypted.encrypted_content_info,
                        password_str.as_bytes(),
                    )
                    .or_else(|| decrypt_pbes2_content(&encrypted.encrypted_content_info, &password))
                }),
            p12::ContentInfo::OtherContext(_) => None,
        };
        let Some(inner) = inner else {
            // Real-JDK's PKCS12KeyStore loads each top-level AuthenticatedSafe
            // section independently and silently drops any section it can't
            // decrypt with the password it was given — this is how
            // `KeyStore.load(stream, null)` can still expose a keystore's
            // PrivateKeyEntry (carried, undecrypted, in an unencrypted outer
            // SafeContents; see the caller's `verify_mac=false` contract) even
            // though a *different* AuthenticatedSafe section (e.g. the
            // certificate SafeContents) is separately encrypted under the
            // store password we don't have. Only tolerate this when we
            // already know we lack a trustworthy password (`verify_mac` was
            // false) — with a MAC-verified password a decrypt failure here is
            // a real bug, not a missing-password situation, and must surface.
            if tolerate_undecryptable {
                continue;
            }
            return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid));
        };
        let safe_bags = yasna::parse_ber(&inner, |r| r.collect_sequence_of(p12::SafeBag::parse))?;
        result.extend(safe_bags);
    }
    Ok(result)
}

/// Decrypt the PBES2/PBKDF2/AES records emitted by current SunPKCS12 for
/// `SecretKeyEntry` values. `p12` itself supports only legacy PKCS#12 PBE.
fn decrypt_pbes2_params(params_der: &[u8], ciphertext: &[u8], password: &[u8]) -> Option<Vec<u8>> {
    let (salt, iterations, key_len, prf, iv) = yasna::parse_ber(params_der, |r| {
        r.read_sequence(|r| {
            let (salt, iterations, key_len, prf) = r.next().read_sequence(|r| {
                let _pbkdf2_oid = r.next().read_oid()?;
                r.next().read_sequence(|r| {
                    let salt = r.next().read_bytes()?;
                    let iterations = r.next().read_u32()?;
                    let key_len = r.read_optional(|r| r.read_u32())?.unwrap_or(32) as usize;
                    let prf = r.read_optional(|r| {
                        r.read_sequence(|r| {
                            let prf_oid = r.next().read_oid()?;
                            r.read_optional(|r| r.read_null())?;
                            Ok(prf_oid)
                        })
                    })?;
                    let prf = match prf.as_ref().map(|oid| oid.components().as_slice()) {
                        Some([1, 2, 840, 113549, 2, 7]) | None => 1,
                        Some([1, 2, 840, 113549, 2, 8]) => 224,
                        Some([1, 2, 840, 113549, 2, 9]) => 256,
                        Some([1, 2, 840, 113549, 2, 10]) => 384,
                        Some([1, 2, 840, 113549, 2, 11]) => 512,
                        _ => return Err(yasna::ASN1Error::new(yasna::ASN1ErrorKind::Invalid)),
                    };
                    Ok((salt, iterations, key_len, prf))
                })
            })?;
            let iv = r.next().read_sequence(|r| {
                let _aes_oid = r.next().read_oid()?;
                r.next().read_bytes()
            })?;
            Ok((salt, iterations, key_len, prf, iv))
        })
    })
    .ok()?;

    let key = crate::phases_early::pbkdf2_derive_for(prf, password, &salt, iterations, key_len);
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    let mut out = vec![0u8; ciphertext.len()];
    let written = match key.len() {
        16 => cbc::Decryptor::<aes::Aes128>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        24 => cbc::Decryptor::<aes::Aes192>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        32 => cbc::Decryptor::<aes::Aes256>::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&iv),
        )
        .decrypt_padded_b2b_mut::<Pkcs7>(&ciphertext, &mut out)
        .ok()?
        .len(),
        _ => return None,
    };
    out.truncate(written);
    Some(out)
}

fn decrypt_pbes2_content(content: &p12::EncryptedContentInfo, password: &[u8]) -> Option<Vec<u8>> {
    let p12::AlgorithmIdentifier::OtherAlg(algorithm) = &content.content_encryption_algorithm
    else {
        return None;
    };
    decrypt_pbes2_params(
        algorithm.params.as_deref()?,
        &content.encrypted_content,
        password,
    )
}

fn decrypt_secret_pbes2(epk: &p12::EncryptedPrivateKeyInfo, password: &[u8]) -> Option<Vec<u8>> {
    let p12::AlgorithmIdentifier::OtherAlg(algorithm) = &epk.encryption_algorithm else {
        return None;
    };
    decrypt_pbes2_params(algorithm.params.as_deref()?, &epk.encrypted_data, password)
}

/// Extend a PKCS#12 key entry's cert chain past its leaf by X.509 issuer/
/// subject matching: repeatedly take the last cert in `chain`, look for
/// another loaded cert (in either `certs_by_local_id` or `orphan_certs`)
/// whose subject DN equals that cert's issuer DN, and append it. Stops when
/// no match is found, the last cert is self-signed (a root), or after a
/// generous hop cap (real chains are a handful of certs deep; this only
/// guards against a malformed/cyclic file spinning forever). Matched certs
/// are removed from their source pool so they end up in exactly one chain,
/// not also flushed later as a standalone `TrustedCert` alias.
fn extend_chain_by_issuer(
    chain: &mut Vec<Vec<u8>>,
    certs_by_local_id: &mut IndexMap<Vec<u8>, Vec<(Option<String>, Vec<u8>)>>,
    orphan_certs: &mut Vec<(Option<String>, Vec<u8>)>,
) {
    const MAX_HOPS: usize = 16;
    for _ in 0..MAX_HOPS {
        let Some(last) = chain.last() else { break };
        let Ok(last_parsed) = crate::x509_manager::parse_certificate(last) else {
            break;
        };
        if last_parsed.issuer_der == last_parsed.subject_der {
            break; // self-signed root; nothing more to append.
        }

        // Search orphan_certs first (the common case: a shared/reused CA
        // cert with no localKeyId of its own), then any remaining
        // local-id-keyed groups.
        let mut found: Option<Vec<u8>> = None;
        if let Some(pos) = orphan_certs.iter().position(|(_, der)| {
            crate::x509_manager::parse_certificate(der)
                .map(|p| p.subject_der == last_parsed.issuer_der)
                .unwrap_or(false)
        }) {
            found = Some(orphan_certs.remove(pos).1);
        } else {
            'outer: for group in certs_by_local_id.values_mut() {
                if let Some(pos) = group.iter().position(|(_, der)| {
                    crate::x509_manager::parse_certificate(der)
                        .map(|p| p.subject_der == last_parsed.issuer_der)
                        .unwrap_or(false)
                }) {
                    found = Some(group.remove(pos).1);
                    break 'outer;
                }
            }
        }

        match found {
            Some(der) => chain.push(der),
            None => break,
        }
    }
    certs_by_local_id.retain(|_, group| !group.is_empty());
}

// ---------------------------------------------------------------------------
// BER -> DER normalisation
// ---------------------------------------------------------------------------
//
// PKCS#12 is a **BER** format, not a DER one, and the `p12` crate this module
// parses with accepts only DER. That is not a theoretical gap: BouncyCastle
// writes indefinite-length constructions, so every PKCS#12 file written by the
// most widely deployed third-party JCA provider was unreadable here.
//
// `openssl asn1parse` on the two, same certificate, same password:
//
// ```text
// BouncyCastle            JDK
//   0:d=0 hl=2 l=inf        0:d=0 hl=4 l= 786   SEQUENCE
//  20:d=3 hl=2 l=inf       26:d=3 hl=4 l= 681   OCTET STRING  (BC's is CONSTRUCTED)
// 669:d=4 hl=2 l=  0                            EOC
// ```
//
// Two BER features are in play and both are handled below:
//
// 1. **Indefinite lengths** - `80` in place of the length, terminated by an
//    end-of-contents `00 00`. Rewritten to the definite form.
// 2. **Segmented strings** - a CONSTRUCTED OCTET STRING whose children are the
//    pieces of one string. DER requires the primitive form, so the pieces are
//    concatenated. This matters beyond parsing: the PKCS#12 MAC is computed
//    over the *contents* of the authSafe OCTET STRING, which is exactly that
//    concatenation.
//
// This is deliberately a LENGTH normalisation and not a general BER-to-DER
// canonicaliser: SET OF ordering and primitive-value canonicalisation are left
// alone, because the parser does not depend on them and rewriting them would
// change bytes the MAC is taken over.

/// The BER indefinite-length marker.
const BER_INDEFINITE: u8 = 0x80;

/// Bound on nesting, so a corrupt or hostile file cannot recurse without end.
const BER_MAX_DEPTH: usize = 64;

struct BerHeader<'a> {
    /// The identifier octets, re-emitted verbatim.
    ident: &'a [u8],
    constructed: bool,
    /// `None` is the indefinite form.
    len: Option<usize>,
}

fn ber_header<'a>(src: &'a [u8], pos: &mut usize) -> Result<BerHeader<'a>, String> {
    let ident_start = *pos;
    let first = *src
        .get(*pos)
        .ok_or_else(|| "truncated identifier".to_string())?;
    *pos += 1;
    if first & 0x1f == 0x1f {
        // High-tag-number form: continues while the top bit is set.
        loop {
            let b = *src
                .get(*pos)
                .ok_or_else(|| "truncated high-tag-number identifier".to_string())?;
            *pos += 1;
            if b & 0x80 == 0 {
                break;
            }
        }
    }
    let ident = &src[ident_start..*pos];
    let l0 = *src
        .get(*pos)
        .ok_or_else(|| "truncated length".to_string())?;
    *pos += 1;
    let len = if l0 == BER_INDEFINITE {
        None
    } else if l0 & 0x80 == 0 {
        Some(l0 as usize)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 8 {
            return Err(format!("unsupported long-form length 0x{l0:02x}"));
        }
        let mut v: usize = 0;
        for _ in 0..n {
            let b = *src
                .get(*pos)
                .ok_or_else(|| "truncated long-form length".to_string())?;
            *pos += 1;
            v = v
                .checked_mul(256)
                .and_then(|x| x.checked_add(b as usize))
                .ok_or_else(|| "length overflows usize".to_string())?;
        }
        Some(v)
    };
    Ok(BerHeader {
        ident,
        constructed: first & 0x20 != 0,
        len,
    })
}

/// Append `len` in the DER definite form (shortest encoding).
fn der_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
        return;
    }
    let mut be = Vec::new();
    let mut v = len;
    while v > 0 {
        be.push((v & 0xff) as u8);
        v >>= 8;
    }
    be.reverse();
    out.push(0x80 | (be.len() as u8));
    out.extend_from_slice(&be);
}

/// A universal OCTET STRING (tag 4), primitive or constructed.
fn ber_is_octet_string(ident: &[u8]) -> bool {
    ident.len() == 1 && ident[0] & 0xc0 == 0x00 && ident[0] & 0x1f == 0x04
}

fn ber_rewrite_one(
    src: &[u8],
    pos: &mut usize,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), String> {
    if depth > BER_MAX_DEPTH {
        return Err(format!("nesting deeper than {BER_MAX_DEPTH}"));
    }
    let header = ber_header(src, pos)?;
    let is_octet_string = ber_is_octet_string(header.ident);

    if !header.constructed {
        let len = header
            .len
            .ok_or_else(|| "primitive value with indefinite length".to_string())?;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| "content length overflows".to_string())?;
        let content = src
            .get(*pos..end)
            .ok_or_else(|| "truncated primitive content".to_string())?;
        out.extend_from_slice(header.ident);
        der_length(len, out);
        out.extend_from_slice(content);
        *pos = end;
        return Ok(());
    }

    // Constructed: rewrite the children first, so the length emitted is the
    // length of the REWRITTEN body rather than the original one.
    let mut inner = Vec::new();
    match header.len {
        Some(len) => {
            let end = pos
                .checked_add(len)
                .ok_or_else(|| "content length overflows".to_string())?;
            if end > src.len() {
                return Err("truncated constructed content".to_string());
            }
            while *pos < end {
                ber_rewrite_one(src, pos, &mut inner, depth + 1)?;
            }
            if *pos != end {
                return Err("child value overran its parent".to_string());
            }
        }
        None => loop {
            let a = *src
                .get(*pos)
                .ok_or_else(|| "truncated indefinite-length value".to_string())?;
            let b = *src
                .get(*pos + 1)
                .ok_or_else(|| "truncated end-of-contents".to_string())?;
            if a == 0x00 && b == 0x00 {
                *pos += 2;
                break;
            }
            ber_rewrite_one(src, pos, &mut inner, depth + 1)?;
        },
    }

    if is_octet_string {
        // Segments of one string: DER wants them joined and primitive. Every
        // child is primitive by now, because a nested constructed OCTET STRING
        // took this same branch.
        let mut joined = Vec::new();
        let mut p = 0usize;
        while p < inner.len() {
            let child = ber_header(&inner, &mut p)?;
            let len = child
                .len
                .ok_or_else(|| "rewritten segment still indefinite".to_string())?;
            let end = p
                .checked_add(len)
                .ok_or_else(|| "segment length overflows".to_string())?;
            joined.extend_from_slice(
                inner
                    .get(p..end)
                    .ok_or_else(|| "truncated segment".to_string())?,
            );
            p = end;
        }
        out.push(header.ident[0] & !0x20);
        der_length(joined.len(), out);
        out.extend_from_slice(&joined);
    } else {
        out.extend_from_slice(header.ident);
        der_length(inner.len(), out);
        out.extend_from_slice(&inner);
    }
    Ok(())
}

/// Rewrite one BER value into the equivalent definite-length encoding.
///
/// A file that is already DER comes back byte-identical, which is what makes
/// this safe as a fallback: the DER path is unchanged.
pub(crate) fn ber_to_definite_length(src: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(src.len());
    ber_rewrite_one(src, &mut pos, &mut out, 0)?;
    Ok(out)
}

pub fn load_pkcs12(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    load_pkcs12_ex(bytes, password, true)
}

/// See [`load_keystore_ex`] — `verify_mac=false` is how a Java `null`
/// password (`KeyStore.load(stream, null)`) reaches this parser. Real-JDK's
/// `PKCS12KeyStore.engineLoad` skips MAC verification entirely in that case
/// ("If a password is not given for integrity checking, then integrity
/// checking is not performed"); treating null the same as an empty `char[]`
/// would instead verify against an empty password and reject every
/// legitimately-passworded store loaded without a keystore password (e.g. a
/// PKCS#12 keystore opened only to read its key entries, whose password is
/// supplied later through `getKey()`).
pub(crate) fn load_pkcs12_ex(
    bytes: &[u8],
    password: &[u8],
    verify_mac: bool,
) -> Result<LoadedKeyStore, KeyStoreError> {
    // DER first, so a conforming file takes exactly the path it always did.
    // A BER one (BouncyCastle writes indefinite lengths and segmented OCTET
    // STRINGs) is normalised and retried - see `ber_to_definite_length`. Both
    // errors are reported if the retry also fails, because "the file is BER"
    // and "the file is corrupt" are different answers.
    let normalised: Vec<u8>;
    let pfx = match p12::PFX::parse(bytes) {
        Ok(pfx) => pfx,
        Err(der_err) => {
            normalised = ber_to_definite_length(bytes).map_err(|ber_err| {
                KeyStoreError::Pkcs12Parse(format!("{der_err:?}; not valid BER either: {ber_err}"))
            })?;
            p12::PFX::parse(&normalised).map_err(|e| {
                KeyStoreError::Pkcs12Parse(format!("{e:?} (after BER normalisation)"))
            })?
        }
    };

    // p12 takes the password as &str (it internally converts to UTF-16BE for
    // PBE-key derivation, matching the PKCS#12 spec). We ask the caller for
    // raw bytes so JKS can use them directly; for PKCS#12 we have to be a
    // valid &str. Anything that came through `char[]` is by definition a
    // valid UTF-16 sequence, so this conversion is lossless for any password
    // a Java caller could possibly produce.
    let password_str = std::str::from_utf8(password)
        .map_err(|_| KeyStoreError::Pkcs12Parse("password not UTF-8".into()))?;
    let password_bmp = pkcs12_bmp_string(password_str);

    // MAC verify (if a MAC is present) before we trust any decrypted bag.
    // Empty passwords MUST verify against an empty input the same way real
    // PKCS12KeyStore does; a Java-null password skips the check altogether.
    if verify_mac && !verify_pkcs12_mac(&pfx, password_str) {
        return Err(KeyStoreError::Pkcs12MacFailed);
    }

    // The `p12` crate parses every layer with yasna::parse_der (strict DER),
    // which rejects the JDK's per-bag attribute SET because the JDK does not
    // DER-sort it (friendlyName is written before the Oracle trustedKeyUsage
    // attribute, but encodes as a larger element so it sorts last). SunJSSE
    // reads it leniently, so we re-implement PFX::bags in BER mode, which
    // relaxes the SET-OF ordering check without skipping any structural
    // validation (kcfull #12).
    let bags = bags_ber(&pfx, password_str, !verify_mac)
        .map_err(|e| KeyStoreError::Pkcs12Parse(format!("bags(): {e:?}")))?;

    // Index bags by `localKeyId` so we can pair a private-key bag with the
    // matching cert chain. Real-JDK uses the same `localKeyId` attribute.
    // `IndexMap`, not `HashMap`: preserves the order bags were encountered in
    // the file, which `entries`'s assembly below relies on to match real
    // JDK's `LinkedHashMap`-backed alias enumeration order (see
    // `LoadedKeyStore::entries`'s doc comment).
    let mut keys_by_local_id: IndexMap<Vec<u8>, (Option<String>, Vec<u8>)> = IndexMap::new();
    let mut certs_by_local_id: IndexMap<Vec<u8>, Vec<(Option<String>, Vec<u8>)>> = IndexMap::new();
    let mut orphan_certs: Vec<(Option<String>, Vec<u8>)> = Vec::new();
    // `Option<String>` for the alias: a bag with no `friendlyName` is numbered
    // from the load-wide counter below, not named here.
    let mut secret_keys: Vec<(Option<String>, Vec<u8>, String)> = Vec::new();

    for bag in &bags {
        let friendly = bag.friendly_name();
        let local_id = bag.local_key_id().unwrap_or_default();

        match &bag.bag {
            p12::SafeBagKind::Pkcs8ShroudedKeyBag(epk) => {
                let legacy = epk.decrypt(&password_bmp);
                let pbes2 = if legacy.is_none() {
                    decrypt_secret_pbes2(epk, password)
                        .or_else(|| decrypt_secret_pbes2(epk, &password_bmp))
                } else {
                    None
                };
                // A PKCS#12 key bag may use a distinct entry password from the
                // store's own load/integrity password (SunPKCS12 only asks for
                // the entry password later, through `getKey()`). Real-JDK still
                // structurally registers the entry as a `PrivateKeyEntry` with
                // its full cert chain in that case -- `isKeyEntry()`/
                // `getCertificateChain()` work without ever decrypting the key,
                // and `getKey()` simply throws `UnrecoverableKeyException` later
                // if the password is wrong. Losing that structure here (by
                // dropping the bag out of `keys_by_local_id` entirely) broke
                // `SslInfo`/`SslMeterBinder`'s chain enumeration: the leaf cert
                // and its issuer chain fell through to the loose-cert-bags flush
                // below and got split into one bogus alias per certificate
                // instead of one `PrivateKeyEntry` alias with an N-cert chain.
                // Mirror the JKS loader's `jks_recover_key(...).unwrap_or(enc_key)`
                // pattern: keep the entry, just with the still-encrypted DER as
                // a placeholder key_der (re-serialized via `EncryptedPrivateKeyInfo
                // ::write`), so alias/chain pairing proceeds identically to a
                // successful decrypt.
                let key_der = legacy.or(pbes2).unwrap_or_else(|| {
                    tracing::debug!(
                        target: "keystore",
                        "deferring separately protected PKCS#12 key bag (kept as encrypted placeholder)"
                    );
                    yasna::construct_der(|w| epk.write(w))
                });
                keys_by_local_id
                    .entry(local_id.clone())
                    .or_insert((friendly, key_der));
            }
            p12::SafeBagKind::CertBag(p12::CertBag::X509(der)) => {
                if local_id.is_empty() {
                    orphan_certs.push((friendly, der.clone()));
                } else {
                    certs_by_local_id
                        .entry(local_id.clone())
                        .or_default()
                        .push((friendly, der.clone()));
                }
            }
            p12::SafeBagKind::OtherBagKind(other) => {
                // SunPKCS12 stores `SecretKeyEntry` values as a SecretBag
                // wrapping an EncryptedPrivateKeyInfo.  p12 0.6 exposes that
                // bag as an opaque OtherBag, so decode its standard inner
                // structure here and retain the encoded secret-key bytes.
                // Some producers encode the encrypted record directly;
                // SunPKCS12 retains the SecretBag sequence and wraps that
                // record in the `[0]` OCTET STRING payload.
                let epki_der =
                    if yasna::parse_ber(&other.bag_value, p12::EncryptedPrivateKeyInfo::parse)
                        .is_ok()
                    {
                        Some(other.bag_value.clone())
                    } else {
                        yasna::parse_ber(&other.bag_value, |r| {
                            r.read_sequence(|r| {
                                let _secret_type = r.next().read_oid()?;
                                r.next()
                                    .read_tagged(yasna::Tag::context(0), |r| r.read_bytes())
                            })
                        })
                        .ok()
                    };
                let secret = epki_der
                    .as_deref()
                    .and_then(|epki_der| {
                        let encrypted =
                            yasna::parse_ber(epki_der, p12::EncryptedPrivateKeyInfo::parse).ok()?;
                        let legacy = encrypted.decrypt(&password_bmp);
                        let pbes2 = if legacy.is_none() {
                            decrypt_secret_pbes2(&encrypted, password)
                        } else {
                            None
                        };
                        legacy.or(pbes2)
                    })
                    .and_then(|secret_info| {
                        yasna::parse_ber(&secret_info, |r| {
                            r.read_sequence(|r| {
                                let _version = r.next().read_u8()?;
                                // NOT discarded (it used to be): this
                                // identifier is the only record of the key's
                                // JCA algorithm name -- see
                                // `EntryKind::SecretKey`.
                                let algorithm = p12::AlgorithmIdentifier::parse(r.next())?;
                                let key_bytes = r.next().read_bytes()?;
                                Ok((algorithm, key_bytes))
                            })
                        })
                        .ok()
                    });
                if let Some((algorithm, key_bytes)) = secret {
                    // `friendly` stays an `Option` here: an un-named bag's alias
                    // is assigned below, from the load-wide counter that has to
                    // number every entry kind in one sequence.
                    secret_keys.push((friendly, key_bytes, secret_alg_name(&algorithm)));
                }
            }
            _ => {}
        }
    }

    // Aliases for bags that carry NO `friendlyName`.
    //
    // The old rule here was "hex of the localKeyId, like keytool does". That is
    // what keytool PRINTS, and it is not what `sun.security.pkcs12.PKCS12KeyStore`
    // — the reader every `KeyStore.getInstance("PKCS12")` actually uses — does.
    // Its `getUnfriendlyName()` is `counter++; return String.valueOf(counter)`,
    // so an un-named bag is called "1", "2", … in bag order, and the counter is
    // shared by every entry kind in one load.
    //
    // Measured on jdk-25.0.3.9-hotspot against netty's own
    // `mutual_auth_server.p12` (a `localKeyID` and no `friendlyName`): HotSpot
    // reports alias `1`; CratonVM reported
    // `70e4faffe3f82e2db1c949282bf86a7c17348838`. Any caller that looks an entry
    // up by the alias its own test data documents then missed —
    // `OpenSslKeyMaterialProviderTest` asks for `"1"` and got null key material
    // (found while closing the retired `ssl-suite-test-discovery-undercounts`
    // write-up, whose fix made the OpenSSL half of those classes run at all).
    let mut unfriendly_counter: u32 = 0;

    let mut entries: IndexMap<String, KeyStoreEntry> = IndexMap::new();

    for (friendly, key_bytes, algorithm) in secret_keys {
        let alias = friendly.unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));
        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::SecretKey {
                    key_bytes,
                    algorithm,
                },
            },
        );
    }

    // Pair keys with their cert chains. Only the leaf cert typically shares
    // the key's `localKeyId` -- SunPKCS12 builds the REST of the chain by
    // repeatedly matching each cert's issuer DN against another loaded
    // cert's subject DN (real X.509 chain-building), not by `localKeyId`.
    // Without this, a multi-cert chain (leaf + intermediate + root) only
    // ever surfaced its leaf here, and the unclaimed issuer certs fell
    // through to the loose-cert-bag flush below as spurious standalone
    // aliases (named by their subject DN, e.g. "CN=ca") instead of being
    // part of this entry's chain -- see `SslMeterBinderTests`'s gauge-count
    // residual (module/spring-boot-micrometer-metrics).
    for (local_id, (key_friendly, key_der)) in keys_by_local_id {
        let mut chain: Vec<Vec<u8>> = Vec::new();
        let mut chain_friendly: Option<String> = None;
        if let Some(matched) = certs_by_local_id.shift_remove(&local_id) {
            for (fn_, der) in matched {
                if chain_friendly.is_none() && fn_.is_some() {
                    chain_friendly = fn_;
                }
                chain.push(der);
            }
        }
        extend_chain_by_issuer(&mut chain, &mut certs_by_local_id, &mut orphan_certs);

        let _ = &local_id;
        let alias = key_friendly
            .or(chain_friendly)
            .unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));

        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::PrivateKey { key_der, chain },
            },
        );
    }

    // Any cert-bags that didn't pair with a key go in as TrustedCert entries.
    let mut walk = certs_by_local_id
        .into_iter()
        .flat_map(|(_, v)| v)
        .collect::<Vec<_>>();
    walk.extend(orphan_certs);
    for (idx, (friendly, der)) in walk.into_iter().enumerate() {
        let _ = idx;
        let alias = friendly.unwrap_or_else(|| unfriendly_alias(&mut unfriendly_counter));
        entries.insert(
            alias.clone(),
            KeyStoreEntry {
                alias,
                creation_time_ms: 0,
                kind: EntryKind::TrustedCert { cert_der: der },
            },
        );
    }

    Ok(LoadedKeyStore { entries })
}

// ---------------------------------------------------------------------------
// JKS — hand-rolled walker
// ---------------------------------------------------------------------------
//
// File layout:
//   u32 magic = 0xFEEDFEED
//   u32 version (1 or 2)
//   u32 entry_count
//   entry_count * {
//     u32 tag (1 = PrivateKeyEntry, 2 = TrustedCertEntry)
//     u16 alias_len + UTF-8 alias  (NB: real JKS uses Java's "modified UTF-8";
//                                   for ASCII aliases the two are identical)
//     u64 creation_date_ms
//     match tag {
//       1 => {
//         u32 enc_key_len + enc_key_bytes (proprietary "JKS encryption" wrapper
//                                          around a PKCS#8 private-key DER —
//                                          we keep it as-is, the receiver of
//                                          the bytes knows the format)
//         u32 chain_count
//         chain_count * {
//           u16 cert_type_len + cert_type ("X.509")
//           u32 cert_der_len + cert_der
//         }
//       }
//       2 => {
//         u16 cert_type_len + cert_type
//         u32 cert_der_len + cert_der
//       }
//     }
//   }
//   [SHA1(password_utf16be || "Mighty Aphrodite" || body)]  // 20 bytes

pub fn load_jks(bytes: &[u8], password: &[u8]) -> Result<LoadedKeyStore, KeyStoreError> {
    if bytes.len() < 4 + 4 + 4 + 20 {
        return Err(KeyStoreError::Truncated(0));
    }

    // Verify the trailing 20-byte SHA-1 integrity tag FIRST, before any
    // structural parsing. Any single-byte flip in the body (e.g. corrupt
    // entry-count) must be rejected as JksMacMismatch — not as a downstream
    // Truncated error from the parser walking off the end of the buffer.
    // RFC: JKS HMAC covers `(password as UTF-16BE) || "Mighty Aphrodite"
    // || body`, where body is everything before the final 20 bytes.
    let body_end = bytes.len() - 20;
    // Real-JDK JavaKeyStore only verifies the integrity HMAC when a password is
    // supplied; a null/empty password loads the certs without the check (the
    // standard way to read a truststore). Mirror that — otherwise loading e.g. a
    // JSSE truststore with no password failed "JKS HMAC integrity check failed"
    // and broke SSLContext creation.
    if !password.is_empty() {
        let stored_mac = &bytes[body_end..];
        let body = &bytes[..body_end];
        let computed = jks_password_mac(password, body);
        if !constant_time_eq(stored_mac, &computed) {
            return Err(KeyStoreError::JksMacMismatch);
        }
    }

    let mut r = JksReader::new(bytes);
    let magic = r.u32_be()?;
    // Either magic: the record layout that follows is identical, and refusing
    // JCEKS here would make the type registered above unreadable by its own
    // writer.
    if magic != JKS_MAGIC && magic != JCEKS_MAGIC {
        return Err(KeyStoreError::BadJksMagic);
    }
    let version = r.u32_be()?;
    if version != 1 && version != 2 {
        return Err(KeyStoreError::BadJksVersion(version));
    }
    let entry_count = r.u32_be()? as usize;

    let mut entries: IndexMap<String, KeyStoreEntry> = IndexMap::new();

    for _ in 0..entry_count {
        let tag = r.u32_be()?;
        let alias = r.utf8_u16len()?;
        let creation_time_ms = r.u64_be()? as i64;

        match tag {
            1 => {
                // PrivateKeyEntry. The stored bytes are the JKS-protected key
                // (an EncryptedPrivateKeyInfo); decrypt it to plaintext PKCS#8
                // via the JKS KeyProtector so downstream consumers (rustls TLS)
                // get a parseable key. If decryption fails (e.g. a per-key
                // password we don't have), keep the raw bytes rather than drop
                // the entry — callers that don't need the key still see it.
                let enc_key = r.bytes_u32len()?;
                let key_der = jks_recover_key(&enc_key, password).unwrap_or(enc_key);
                let chain_count = r.u32_be()? as usize;
                // SECURITY: `chain_count` is attacker-controlled. A forged
                // truststore (especially one loaded with an empty password,
                // which skips the integrity MAC) could declare e.g.
                // 0xFFFFFFFF certs and force `Vec::with_capacity(4G)` → OOM
                // DoS before a single cert is parsed. Each chain element
                // costs at minimum a u16 cert-type length (2 bytes) + a u32
                // cert-der length (4 bytes) = 6 bytes in the stream, so a
                // count larger than `remaining / 6` is structurally
                // impossible — reject it as truncated rather than trusting it.
                // We also cap the *reserved* capacity (not the loop bound) at
                // the same realistic ceiling so we never pre-reserve for more
                // certs than the buffer can physically contain, and let the
                // per-element `need()` checks in the loop do the final
                // enforcement (Vec grows incrementally via `push`).
                const JKS_CHAIN_MIN_ELEM_BYTES: usize = 6; // u16 type-len + u32 der-len
                let max_possible_chain = r.remaining() / JKS_CHAIN_MIN_ELEM_BYTES;
                if chain_count > max_possible_chain {
                    return Err(KeyStoreError::Truncated(r.pos()));
                }
                let mut chain = Vec::with_capacity(chain_count.min(max_possible_chain));
                for _ in 0..chain_count {
                    let _cert_type = r.utf8_u16len()?;
                    let cert_der = r.bytes_u32len()?;
                    chain.push(cert_der);
                }
                entries.insert(
                    alias.clone(),
                    KeyStoreEntry {
                        alias,
                        creation_time_ms,
                        kind: EntryKind::PrivateKey { key_der, chain },
                    },
                );
            }
            2 => {
                // TrustedCertEntry
                if version == 2 {
                    let _cert_type = r.utf8_u16len()?;
                }
                // v1 stores cert directly; v2 wraps with cert_type prefix.
                // For v1 we still read the (cert_type) prefix because real
                // OpenJDK source does so unconditionally — the version
                // distinction here is for *tag-3* (sealed) entries which
                // we don't support. Keep one path:
                if version == 1 {
                    let _cert_type = r.utf8_u16len()?;
                }
                let cert_der = r.bytes_u32len()?;
                entries.insert(
                    alias.clone(),
                    KeyStoreEntry {
                        alias,
                        creation_time_ms,
                        kind: EntryKind::TrustedCert { cert_der },
                    },
                );
            }
            other => return Err(KeyStoreError::BadJksTag(other)),
        }
    }

    // HMAC was already verified at the top of this function, so any
    // structural parse that reached here is trustworthy. Defense-in-depth
    // sanity check: parser must have consumed exactly `body_end` bytes
    // (i.e. body length declared by the HMAC envelope must match the
    // entries we parsed). Mismatch here means the body contains trailing
    // padding/garbage despite a valid HMAC — reject as malformed.
    if r.pos() != body_end {
        return Err(KeyStoreError::Truncated(r.pos()));
    }
    Ok(LoadedKeyStore { entries })
}

/// PBES2 / PBKDF2-HMAC-SHA256 / AES-256-CBC encryption of one PKCS#8-shaped
/// blob, producing the `EncryptedPrivateKeyInfo` a PKCS#12 shrouded key bag
/// or secret bag carries.
///
/// This is the exact inverse of [`decrypt_pbes2_params`] above, and the same
/// envelope current `SunPKCS12` writes by default
/// (`keystore.pkcs12.keyProtectionAlgorithm` = `PBEWithHMACSHA256AndAES_256`),
/// so the output is readable by both this module's loader and a real JDK.
///
/// Returns `None` when OS entropy is unavailable -- a predictable salt/IV here
/// would be worse than refusing, and [`write_pkcs12`] turns the `None` into a
/// visible error rather than an unprotected or silently-dropped key.
fn pbes2_encrypt(plain: &[u8], password: &[u8]) -> Option<p12::EncryptedPrivateKeyInfo> {
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};

    // 10_000 rounds is SunPKCS12's own default for this algorithm; matching it
    // keeps a CV-written keystore indistinguishable from a keytool-written one
    // in the one parameter a reader is entitled to sanity-check.
    const ITERATIONS: u32 = 10_000;
    const KEY_LEN: usize = 32;

    let mut salt = [0u8; 20];
    let mut iv = [0u8; 16];
    if !crate::securerandom::os_random_bytes(&mut salt) {
        return None;
    }
    if !crate::securerandom::os_random_bytes(&mut iv) {
        return None;
    }

    // prf 256 == HMAC-SHA256, the encoding `decrypt_pbes2_params` uses.
    let key = crate::phases_early::pbkdf2_derive_for(256, password, &salt, ITERATIONS, KEY_LEN);
    let mut out = vec![0u8; plain.len() + 16];
    let written = cbc::Encryptor::<aes::Aes256>::new(
        GenericArray::from_slice(&key),
        GenericArray::from_slice(&iv),
    )
    .encrypt_padded_b2b_mut::<Pkcs7>(plain, &mut out)
    .ok()?
    .len();
    out.truncate(written);

    let params = yasna::construct_der(|w| {
        w.write_sequence(|w| {
            // keyDerivationFunc: PBKDF2 { salt, iterationCount, keyLength, prf }
            w.next().write_sequence(|w| {
                w.next()
                    .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                        1, 2, 840, 113549, 1, 5, 12,
                    ]));
                w.next().write_sequence(|w| {
                    w.next().write_bytes(&salt);
                    w.next().write_u32(ITERATIONS);
                    w.next().write_u32(KEY_LEN as u32);
                    w.next().write_sequence(|w| {
                        w.next()
                            .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                                1, 2, 840, 113549, 2, 9,
                            ]));
                        w.next().write_null();
                    });
                });
            });
            // encryptionScheme: aes256-CBC { iv }
            w.next().write_sequence(|w| {
                w.next()
                    .write_oid(&yasna::models::ObjectIdentifier::from_slice(&[
                        2, 16, 840, 1, 101, 3, 4, 1, 42,
                    ]));
                w.next().write_bytes(&iv);
            });
        })
    });

    Some(p12::EncryptedPrivateKeyInfo {
        encryption_algorithm: p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
            // PBES2
            algorithm_type: yasna::models::ObjectIdentifier::from_slice(&[
                1, 2, 840, 113549, 1, 5, 13,
            ]),
            params: Some(params),
        }),
        encrypted_data: out,
    })
}

/// Serialise a keystore to PKCS#12 (RFC 7292).
///
/// WHY THIS EXISTS AT ALL: JKS -- what [`write_jks`] emits, and what
/// `engineStore` wrote for every store regardless of declared type -- has no
/// representation for a `SecretKeyEntry`. A keystore holding one therefore had
/// to either change format or lose the entry, and it silently lost it: the
/// alias was filtered out of the JKS body and the caller got a structurally
/// valid, entry-less file plus no exception
/// (`docs/known-issues/vm/pkcs12-setentry-secretkeyentry-is-a-silent-noop-20260805.md`).
///
/// Shape, and why each choice: one unencrypted `SafeContents`
/// (`ContentInfo::Data`) carrying every bag; private keys in a
/// `pkcs8ShroudedKeyBag` and secret keys in a `secretBag`, both wrapped in the
/// PBES2 envelope [`pbes2_encrypt`] builds; certificates in the clear inside
/// that same SafeContents. `SunPKCS12` additionally encrypts the certificate
/// SafeContents -- nothing requires it, certificates are public, and leaving
/// them readable is what keeps a CV-written store listable by a plain
/// `keytool -list` with no password. Integrity is a SHA-256 `MacData` over the
/// AuthenticatedSafe, which is what [`verify_pkcs12_mac`] checks on the way
/// back in.
///
/// Cert-bag attributes mirror `SunPKCS12` exactly: the LEAF of a key entry's
/// chain carries `localKeyId` + `friendlyName`, the rest of the chain carries
/// nothing and is re-attached on load by issuer/subject matching
/// ([`extend_chain_by_issuer`]). Giving every chain cert the same `localKeyId`
/// would read back correctly here but is not what a real JDK expects to find.
///
/// KNOWN LIMITATION, stated rather than hidden: PKCS#12 permits a per-entry
/// key password distinct from the store password, and a `LoadedKeyStore` does
/// not carry one -- every key is protected with the STORE password, exactly
/// like [`jks_protect_key`] does for JKS.
pub(crate) fn write_pkcs12(store: &LoadedKeyStore, password: &[u8]) -> Result<Vec<u8>, String> {
    use yasna::models::ObjectIdentifier;

    let oid_pkcs8_shrouded = ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 12, 10, 1, 2]);
    let oid_secret_bag = ObjectIdentifier::from_slice(&[1, 2, 840, 113549, 1, 12, 10, 1, 5]);

    let mut bags: Vec<p12::SafeBag> = Vec::new();
    let mut next_key_id: u32 = 0;

    for (alias, entry) in store.entries.iter() {
        match &entry.kind {
            EntryKind::TrustedCert { cert_der } => {
                // The `trustedKeyUsage` attribute is what makes this a TRUSTED
                // certificate entry rather than a stray certificate. Without
                // it, a real JDK reading the file drops the bag entirely (it
                // keeps only certs that pair with a key), which is how the
                // first cut of this writer produced a 4-entry keystore that
                // HotSpot listed as 3 — silent loss of exactly the kind this
                // record is about. `AnyUsage` (2.5.29.37.0) is the value
                // `PKCS12KeyStore.setCertEntry` writes.
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(cert_der.clone())),
                    attributes: vec![
                        p12::PKCS12Attribute::FriendlyName(alias.clone()),
                        p12::PKCS12Attribute::Other(p12::OtherAttribute {
                            oid: ObjectIdentifier::from_slice(&[
                                2, 16, 840, 1, 113894, 746875, 1, 1,
                            ]),
                            data: vec![yasna::construct_der(|w| {
                                w.write_oid(&ObjectIdentifier::from_slice(&[2, 5, 29, 37, 0]))
                            })],
                        }),
                    ],
                });
            }
            EntryKind::PrivateKey { key_der, chain } => {
                next_key_id += 1;
                let key_id = next_key_id.to_be_bytes().to_vec();
                // A key that never decrypted is still inside its ORIGINAL
                // envelope (see `load_jks`/`load_pkcs12`'s
                // `unwrap_or(encrypted)` arms). Re-wrapping it would
                // double-encrypt; pass it straight through when it already
                // parses as an EncryptedPrivateKeyInfo.
                let epki = if let Ok(existing) =
                    yasna::parse_ber(key_der, p12::EncryptedPrivateKeyInfo::parse)
                {
                    existing
                } else {
                    pbes2_encrypt(key_der, password).ok_or_else(|| {
                        format!(
                            "write_pkcs12({alias:?}): OS entropy unavailable, refusing to \\n                             protect a private key with a predictable salt"
                        )
                    })?
                };
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::Pkcs8ShroudedKeyBag(epki),
                    attributes: vec![
                        p12::PKCS12Attribute::FriendlyName(alias.clone()),
                        p12::PKCS12Attribute::LocalKeyId(key_id.clone()),
                    ],
                });
                for (idx, cert_der) in chain.iter().enumerate() {
                    bags.push(p12::SafeBag {
                        bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(cert_der.clone())),
                        attributes: if idx == 0 {
                            vec![
                                p12::PKCS12Attribute::FriendlyName(alias.clone()),
                                p12::PKCS12Attribute::LocalKeyId(key_id.clone()),
                            ]
                        } else {
                            Vec::new()
                        },
                    });
                }
            }
            EntryKind::SecretKey {
                key_bytes,
                algorithm,
            } => {
                let Some(alg_oid) = secret_key_alg_oid(algorithm) else {
                    return Err(format!(
                        "write_pkcs12({alias:?}): no PKCS#12 OID is known for secret-key \\n                         algorithm {algorithm:?}, and writing it under a wrong OID would \\n                         read back as a different algorithm"
                    ));
                };
                // The PKCS#8-shaped plaintext SunPKCS12 encrypts:
                //   SEQUENCE { INTEGER 0, AlgorithmIdentifier, OCTET STRING key }
                let plain = yasna::construct_der(|w| {
                    w.write_sequence(|w| {
                        w.next().write_u8(0);
                        w.next().write_sequence(|w| {
                            w.next().write_oid(&alg_oid);
                        });
                        w.next().write_bytes(key_bytes);
                    })
                });
                let epki = pbes2_encrypt(&plain, password).ok_or_else(|| {
                    format!(
                        "write_pkcs12({alias:?}): OS entropy unavailable, refusing to protect \\n                         a secret key with a predictable salt"
                    )
                })?;
                let epki_der = yasna::construct_der(|w| epki.write(w));
                // SecretBag ::= SEQUENCE { secretTypeId, [0] EXPLICIT ANY }.
                // SunPKCS12 writes pkcs8ShroudedKeyBag as the type id and the
                // EncryptedPrivateKeyInfo as an OCTET STRING inside the tag.
                let secret_bag = yasna::construct_der(|w| {
                    w.write_sequence(|w| {
                        w.next().write_oid(&oid_pkcs8_shrouded);
                        w.next()
                            .write_tagged(yasna::Tag::context(0), |w| w.write_bytes(&epki_der));
                    })
                });
                bags.push(p12::SafeBag {
                    bag: p12::SafeBagKind::OtherBagKind(p12::OtherBag {
                        bag_id: oid_secret_bag.clone(),
                        bag_value: secret_bag,
                    }),
                    attributes: vec![p12::PKCS12Attribute::FriendlyName(alias.clone())],
                });
            }
        }
    }

    let safe_contents = yasna::construct_der(|w| {
        w.write_sequence_of(|w| {
            for bag in &bags {
                bag.write(w.next());
            }
        })
    });
    let auth_safe = yasna::construct_der(|w| {
        w.write_sequence_of(|w| {
            p12::ContentInfo::Data(safe_contents.clone()).write(w.next());
        })
    });

    // MacData over the AuthenticatedSafe. SHA-256, because that is what
    // `pkcs12_mac_digest` names as the default current SunPKCS12 emits and
    // what `verify_pkcs12_mac` will re-derive on the way back in.
    const MAC_ITERATIONS: u32 = 10_000;
    let mut mac_salt = [0u8; 20];
    if !crate::securerandom::os_random_bytes(&mut mac_salt) {
        return Err(
            "write_pkcs12: OS entropy unavailable, refusing to emit a PKCS#12 MAC with a \\n             predictable salt"
                .to_string(),
        );
    }
    let password_str = String::from_utf8_lossy(password).to_string();
    let password_bmp = pkcs12_bmp_string(&password_str);
    let digest = Pkcs12MacDigest::Sha256;
    let mac_key = pkcs12_mac_kdf(
        digest,
        &password_bmp,
        &mac_salt,
        MAC_ITERATIONS,
        3,
        digest.output_len(),
    );
    let mac_data = p12::MacData {
        mac: p12::DigestInfo {
            digest_algorithm: p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
                algorithm_type: ObjectIdentifier::from_slice(&[2, 16, 840, 1, 101, 3, 4, 2, 1]),
                params: Some(yasna::construct_der(|w| w.write_null())),
            }),
            digest: pkcs12_hmac(digest, &mac_key, &auth_safe),
        },
        salt: mac_salt.to_vec(),
        iterations: MAC_ITERATIONS,
    };

    let pfx = p12::PFX {
        version: 3,
        auth_safe: p12::ContentInfo::Data(auth_safe),
        mac_data: Some(mac_data),
    };
    Ok(pfx.to_der())
}

/// Serialise a keystore to the JKS v2 wire format (magic, version, entry
/// count, per-entry records, trailing `SHA1(pw||salt||body)` tag). Used by
/// `engineStore`: CV's read path (`load_keystore`) detects the format by magic,
/// so writing JKS round-trips through CV's own JKS parser regardless of the
/// `KeyStore` type the caller declared (the keycloak truststore round-trip
/// stores as "PKCS12" but is detected/loaded by content). Mirrors `load_jks`'s
/// record layout exactly. Trusted certs are written as tag-2 entries; private
/// keys as tag-1 wrapped in Sun's `KeyProtector` `EncryptedPrivateKeyInfo`
/// envelope (see `jks_protect_key`), which is the exact inverse of the
/// `jks_recover_key` call `load_jks` makes on the way back in.
pub(crate) fn write_jks(store: &LoadedKeyStore, password: &[u8]) -> Vec<u8> {
    write_jks_with_magic(store, password, JKS_MAGIC)
}

/// [`write_jks`] under an explicit magic, so JCEKS can share the record writer.
///
/// The two formats differ in the magic and in how a private key is protected;
/// this writer does not protect private keys differently per format, so the
/// magic is the whole difference it can express -- and writing a JCEKS store
/// under the JKS magic would produce a file `keytool -storetype JCEKS` refuses.
pub(crate) fn write_jks_with_magic(
    store: &LoadedKeyStore,
    password: &[u8],
    magic: u32,
) -> Vec<u8> {
    let cert_type: &[u8] = b"X.509";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&magic.to_be_bytes());
    body.extend_from_slice(&2u32.to_be_bytes()); // version 2
                                                 // JKS has no compatible representation for SecretKeyEntry.  Keep the
                                                 // entry available in memory, but omit it from this legacy wire format.
                                                 // (PKCS#12 callers are still loaded from their original SecretBag.)
    let aliases: Vec<&String> = store
        .entries
        .iter()
        .filter_map(|(alias, entry)| match &entry.kind {
            EntryKind::SecretKey { .. } => None,
            _ => Some(alias),
        })
        .collect();
    body.extend_from_slice(&(aliases.len() as u32).to_be_bytes());

    // INSERTION order, not alphabetical. `LoadedKeyStore::entries` is an
    // `IndexMap` precisely because real JDK enumerates a keystore in the order
    // its entries were read/inserted, and the `sort_unstable()` that used to
    // stand here made a store -> load round trip silently re-alphabetise them:
    // `setKeyEntry("pk", ..); setCertificateEntry("ca", ..)` came back as
    // `[ca, pk]` where HotSpot answers `[pk, ca]`. `IndexMap` iteration is
    // itself deterministic, which is all that sort was reaching for.
    for alias in aliases {
        let entry = &store.entries[alias];
        let ab = alias.as_bytes();
        match &entry.kind {
            EntryKind::TrustedCert { cert_der } => {
                body.extend_from_slice(&2u32.to_be_bytes()); // tag = TrustedCertEntry
                body.extend_from_slice(&(ab.len() as u16).to_be_bytes());
                body.extend_from_slice(ab);
                body.extend_from_slice(&(entry.creation_time_ms as u64).to_be_bytes());
                body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
                body.extend_from_slice(cert_type);
                body.extend_from_slice(&(cert_der.len() as u32).to_be_bytes());
                body.extend_from_slice(cert_der);
            }
            EntryKind::PrivateKey { key_der, chain } => {
                // Re-apply Sun's KeyProtector envelope. `load_jks` stores the
                // DECRYPTED PKCS#8 (see its tag-1 arm), so writing `key_der`
                // straight out — the previous behaviour — put an UNPROTECTED
                // private key on disk in a file the caller had just supplied a
                // password for, and produced a file no real JDK could read.
                // Two pass-through cases stay byte-identical to before:
                //   * an entry whose key never decrypted (wrong/absent key
                //     password) still holds the ORIGINAL envelope — re-wrapping
                //     it would double-encrypt;
                //   * an entropy failure, where refusing to write anything at
                //     all would lose the entry; that path keeps the old
                //     behaviour and warns loudly rather than emitting a
                //     predictable-salt envelope.
                let protected: Vec<u8> = if is_jks_encrypted_private_key(key_der) {
                    key_der.clone()
                } else {
                    match jks_protect_key(key_der, password) {
                        Some(wrapped) => wrapped,
                        None => {
                            tracing::warn!(
                                target: "keystore",
                                alias = %alias,
                                "OS entropy unavailable; JKS private key written UNPROTECTED"
                            );
                            key_der.clone()
                        }
                    }
                };
                body.extend_from_slice(&1u32.to_be_bytes()); // tag = PrivateKeyEntry
                body.extend_from_slice(&(ab.len() as u16).to_be_bytes());
                body.extend_from_slice(ab);
                body.extend_from_slice(&(entry.creation_time_ms as u64).to_be_bytes());
                body.extend_from_slice(&(protected.len() as u32).to_be_bytes());
                body.extend_from_slice(&protected);
                body.extend_from_slice(&(chain.len() as u32).to_be_bytes());
                for c in chain {
                    body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
                    body.extend_from_slice(cert_type);
                    body.extend_from_slice(&(c.len() as u32).to_be_bytes());
                    body.extend_from_slice(c);
                }
            }
            EntryKind::SecretKey { .. } => unreachable!("secret keys are filtered above"),
        }
    }
    let mac = jks_password_mac(password, &body);
    body.extend_from_slice(&mac);
    body
}

/// JKS integrity tag: `SHA1(password_utf16be || "Mighty Aphrodite" || body)`.
///
/// This is *not* a standard HMAC. Sun/OpenJDK's `JavaKeyStore` invented this
/// construction in JDK 1.2 and it has been frozen since. Empty passwords
/// produce an empty UTF-16 prefix (the SHA-1 starts straight from the salt
/// + body).
fn jks_password_mac(password_bytes: &[u8], body: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    // Treat each input byte as a Latin-1 codepoint and emit UTF-16BE: the
    // high byte is 0, the low byte is the original. This matches what the
    // JDK does for the common ASCII case (Java `char[]` produced by
    // `password.toCharArray()` where each char is `<= 0x00FF`). For exotic
    // passwords callers would have to pass the bytes already in UTF-16BE
    // form; that path is rarely exercised.
    for b in password_bytes {
        hasher.update([0u8, *b]);
    }
    hasher.update(JKS_HMAC_SALT);
    hasher.update(body);
    let out = hasher.finalize();
    let mut tag = [0u8; 20];
    tag.copy_from_slice(&out);
    tag
}

/// Encode a password (bytes treated as Latin-1 codepoints) as UTF-16BE, the
/// form JKS feeds to SHA-1 (high byte 0, low byte original).
fn jks_passwd_utf16be(password_bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(password_bytes.len() * 2);
    for b in password_bytes {
        v.push(0u8);
        v.push(*b);
    }
    v
}

/// Minimal DER walker: read a length field at `pos`, returning (length, new_pos).
fn der_read_len(data: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    let first = *data.get(pos)?;
    pos += 1;
    if first < 0x80 {
        return Some((first as usize, pos));
    }
    let n = (first & 0x7f) as usize;
    if n == 0 || n > 4 {
        return None;
    }
    let mut len = 0usize;
    for _ in 0..n {
        len = (len << 8) | (*data.get(pos)? as usize);
        pos += 1;
    }
    Some((len, pos))
}

/// Extract the `encryptedData` OCTET STRING from an `EncryptedPrivateKeyInfo`
/// DER: `SEQUENCE { AlgorithmIdentifier, OCTET STRING }`. Returns the octet
/// content (for JKS: `salt(20) || encryptedKey || digest(20)`).
fn der_extract_epki_octets(der: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    if *der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let (_, p) = der_read_len(der, pos)?;
    pos = p;
    // AlgorithmIdentifier SEQUENCE — skip whole.
    if *der.get(pos)? != 0x30 {
        return None;
    }
    pos += 1;
    let (alg_len, p) = der_read_len(der, pos)?;
    pos = p + alg_len;
    // OCTET STRING.
    if *der.get(pos)? != 0x04 {
        return None;
    }
    pos += 1;
    let (oct_len, p) = der_read_len(der, pos)?;
    pos = p;
    der.get(pos..pos + oct_len).map(|s| s.to_vec())
}

/// Recover a JKS-protected private key into its plaintext PKCS#8 DER.
///
/// JKS wraps the PKCS#8 key in an `EncryptedPrivateKeyInfo` whose OCTET STRING
/// is `salt(20) || (plainPkcs8 XOR keystream) || SHA1(passwd||plainPkcs8)`,
/// where the keystream is `Wi = SHA1(passwdUtf16be || W(i-1))`, `W0 = salt`
/// (Sun's frozen JDK-1.2 `KeyProtector`). Returns the plaintext PKCS#8 DER, or
/// `None` if the structure / integrity check doesn't hold.
fn jks_recover_key(epki_der: &[u8], password_bytes: &[u8]) -> Option<Vec<u8>> {
    use sha1::{Digest, Sha1};
    let protected = der_extract_epki_octets(epki_der)?;
    if protected.len() < 40 {
        return None;
    }
    let salt = &protected[..20];
    let encr_len = protected.len() - 40;
    let encr_key = &protected[20..20 + encr_len];
    let check = &protected[20 + encr_len..];
    let pw = jks_passwd_utf16be(password_bytes);

    let mut xor_key = Vec::with_capacity(encr_len);
    let mut digest: Vec<u8> = salt.to_vec();
    while xor_key.len() < encr_len {
        let mut h = Sha1::new();
        h.update(&pw);
        h.update(&digest);
        digest = h.finalize().to_vec();
        xor_key.extend_from_slice(&digest);
    }
    let plain: Vec<u8> = encr_key
        .iter()
        .zip(xor_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    // Integrity: SHA1(passwd || plain) must equal the trailing digest.
    let mut hc = Sha1::new();
    hc.update(&pw);
    hc.update(&plain);
    let computed = hc.finalize();
    if !constant_time_eq(&computed, check) {
        tracing::warn!(target: "keystore", "JKS key integrity check failed (wrong password?)");
        return None;
    }
    Some(plain)
}

/// DER length prefix for `n` content bytes (short form under 128, else the
/// minimal long form). Only lengths up to 2^24-1 occur here (a private key is
/// a few kilobytes at most).
fn der_len_bytes(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else if n <= 0xFF {
        vec![0x81, n as u8]
    } else if n <= 0xFFFF {
        vec![0x82, (n >> 8) as u8, n as u8]
    } else {
        vec![0x83, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    }
}

/// `AlgorithmIdentifier { 1.3.6.1.4.1.42.2.17.1.1, NULL }` — Sun's frozen
/// JDK-1.2 `KeyProtector` algorithm id, the only one a JKS `PrivateKeyEntry`
/// ever carries. Written out verbatim; `der_extract_epki_octets` (the read
/// side) skips the whole SEQUENCE, and a real JDK's `AlgorithmId.parse`
/// accepts the explicit NULL parameters.
const JKS_KEY_PROTECTOR_ALG_ID: [u8; 16] = [
    0x30, 0x0E, 0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x2A, 0x02, 0x11, 0x01, 0x01, 0x05, 0x00,
];

/// Wrap a plaintext PKCS#8 private key in Sun's JKS `KeyProtector` envelope —
/// the exact inverse of [`jks_recover_key`].
///
/// STUB-REMOVAL (wave 2) / wave-1 follow-up: `write_jks` used to emit the
/// PLAINTEXT PKCS#8 bytes where the format (and every real JDK reading the
/// file) requires an `EncryptedPrivateKeyInfo`. Two consequences, both bad:
/// a keystore CratonVM wrote could not be read back by a real JDK at all, and
/// — far worse — `KeyStore.store()` silently wrote unprotected private keys to
/// disk for a caller who had just supplied a password precisely to prevent
/// that.
///
/// Envelope: `SEQUENCE { AlgorithmIdentifier, OCTET STRING }` where the octets
/// are `salt(20) || (plainPkcs8 XOR keystream) || SHA1(passwdUtf16be ||
/// plainPkcs8)` and the keystream is `Wi = SHA1(passwdUtf16be || W(i-1))` with
/// `W0 = salt`.
///
/// Returns `None` when the OS entropy source is unavailable, so the caller can
/// decide what to do rather than fall back to a predictable salt.
///
/// KNOWN LIMITATION (documented, not silently papered over): JKS permits a
/// per-entry key password distinct from the store password, but a loaded
/// `KeyStoreEntry` does not carry one — `load_jks` decrypts with whatever
/// password it was given and keeps only the plaintext. `write_jks` therefore
/// protects every key with the STORE password, which is correct for the
/// overwhelmingly common (and every fixture's) case where the two are equal,
/// and changes the key password to the store password otherwise.
fn jks_protect_key(plain_pkcs8: &[u8], password_bytes: &[u8]) -> Option<Vec<u8>> {
    use sha1::{Digest, Sha1};
    let mut salt = [0u8; 20];
    if !crate::securerandom::os_random_bytes(&mut salt) {
        return None;
    }
    let pw = jks_passwd_utf16be(password_bytes);

    let mut xor_key: Vec<u8> = Vec::with_capacity(plain_pkcs8.len() + 20);
    let mut digest: Vec<u8> = salt.to_vec();
    while xor_key.len() < plain_pkcs8.len() {
        let mut h = Sha1::new();
        h.update(&pw);
        h.update(&digest);
        digest = h.finalize().to_vec();
        xor_key.extend_from_slice(&digest);
    }
    let cipher: Vec<u8> = plain_pkcs8
        .iter()
        .zip(xor_key.iter())
        .map(|(a, b)| a ^ b)
        .collect();

    let mut hc = Sha1::new();
    hc.update(&pw);
    hc.update(plain_pkcs8);
    let check = hc.finalize();

    let mut protected = Vec::with_capacity(20 + cipher.len() + 20);
    protected.extend_from_slice(&salt);
    protected.extend_from_slice(&cipher);
    protected.extend_from_slice(&check);

    let mut octet = Vec::with_capacity(protected.len() + 5);
    octet.push(0x04);
    octet.extend_from_slice(&der_len_bytes(protected.len()));
    octet.extend_from_slice(&protected);

    let content_len = JKS_KEY_PROTECTOR_ALG_ID.len() + octet.len();
    let mut out = Vec::with_capacity(content_len + 5);
    out.push(0x30);
    out.extend_from_slice(&der_len_bytes(content_len));
    out.extend_from_slice(&JKS_KEY_PROTECTOR_ALG_ID);
    out.extend_from_slice(&octet);
    Some(out)
}

/// Whether a JKS key entry is still wrapped in Sun's KeyProtector envelope.
pub(crate) fn is_jks_encrypted_private_key(der: &[u8]) -> bool {
    const JKS_KEY_PROTECTOR_OID: &[u8] = b"\x06\x0a\x2b\x06\x01\x04\x01\x2a\x02\x11\x01\x01";
    der.windows(JKS_KEY_PROTECTOR_OID.len())
        .any(|window| window == JKS_KEY_PROTECTOR_OID)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

struct JksReader<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> JksReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }
    fn pos(&self) -> usize {
        self.cursor
    }
    /// Bytes left in the buffer from the current cursor. Used to bound
    /// attacker-controlled element counts before reserving capacity, so a
    /// forged count can never drive a huge `Vec::with_capacity` (OOM DoS).
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }
    fn need(&self, n: usize) -> Result<(), KeyStoreError> {
        if self.cursor + n > self.data.len() {
            Err(KeyStoreError::Truncated(self.cursor))
        } else {
            Ok(())
        }
    }
    fn u32_be(&mut self) -> Result<u32, KeyStoreError> {
        self.need(4)?;
        let v = u32::from_be_bytes([
            self.data[self.cursor],
            self.data[self.cursor + 1],
            self.data[self.cursor + 2],
            self.data[self.cursor + 3],
        ]);
        self.cursor += 4;
        Ok(v)
    }
    fn u64_be(&mut self) -> Result<u64, KeyStoreError> {
        self.need(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.data[self.cursor..self.cursor + 8]);
        self.cursor += 8;
        Ok(u64::from_be_bytes(buf))
    }
    fn u16_be(&mut self) -> Result<u16, KeyStoreError> {
        self.need(2)?;
        let v = u16::from_be_bytes([self.data[self.cursor], self.data[self.cursor + 1]]);
        self.cursor += 2;
        Ok(v)
    }
    fn utf8_u16len(&mut self) -> Result<String, KeyStoreError> {
        let len = self.u16_be()? as usize;
        self.need(len)?;
        let s = String::from_utf8_lossy(&self.data[self.cursor..self.cursor + len]).into_owned();
        self.cursor += len;
        Ok(s)
    }
    fn bytes_u32len(&mut self) -> Result<Vec<u8>, KeyStoreError> {
        let len = self.u32_be()? as usize;
        self.need(len)?;
        let v = self.data[self.cursor..self.cursor + len].to_vec();
        self.cursor += len;
        Ok(v)
    }
}

/// The alias `sun.security.pkcs12.PKCS12KeyStore` gives a bag that carries no
/// `friendlyName`: `getUnfriendlyName()`, i.e. `++counter` rendered as a
/// decimal string, shared across every entry kind in one load.
///
/// See the call sites in `parse_pkcs12` for why the previous "hex of the
/// localKeyId" rule (what *keytool* prints, not what the reader assigns) made
/// every alias in netty's `mutual_auth_server.p12` unfindable.
fn unfriendly_alias(counter: &mut u32) -> String {
    *counter += 1;
    counter.to_string()
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{:02x}", x));
    }
    s
}

// ---------------------------------------------------------------------------
// Native registration
// ---------------------------------------------------------------------------

/// Every class the provider table advertises as a `KeyStore` implementation,
/// so the guard below and the registrations above cannot drift apart silently.
#[cfg(test)]
const ADVERTISED_KEYSTORE_CLASSES: &[&str] = &[
    "sun/security/provider/JavaKeyStore$DualFormatJKS",
    "sun/security/provider/JavaKeyStore$CaseExactJKS",
    "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12",
    "sun/security/provider/DomainKeyStore$DKS",
    "sun/security/pkcs12/PKCS12KeyStore",
    // SunJCE's `KeyStore.JCEKS`. The row in `provider_chain`, this one and the
    // `register_engine_surface` call are joined only by a string, which is what
    // `advertised_keystore_registration_tests` below exists to check -- see its
    // doc comment for the 754-to-563 regression that gap once caused, and note
    // that it caught this entry on the day it was added, with the engine
    // surface missing.
    JCEKS_FQN,
];

#[cfg(test)]
mod advertised_keystore_registration_tests {
    use super::ADVERTISED_KEYSTORE_CLASSES;

    /// A `KeyStore` row the provider table advertises must resolve to a class
    /// this module actually serves.
    ///
    /// The two are joined only by a string, and dispatch here is by CLASS NAME
    /// with no inheritance walk — so correcting a row to the real JDK class
    /// name, which is exactly the right thing to do, silently unregisters the
    /// engine surface. That happened: SUN's `KeyStore.PKCS12` was corrected
    /// from `PKCS12KeyStore` to `PKCS12KeyStore$DualFormatPKCS12`, the JKS
    /// twin of the same change WAS registered, and the PKCS12 one was not.
    /// `KeyStore.getInstance("PKCS12")` — the default — then loaded nothing,
    /// and netty's `JdkSslEngineTest` went from 754 passing to 563.
    ///
    /// Nothing in the build said so. The provider table was right, the
    /// registration list was right for what it listed, and the gap was between
    /// them. This test is that gap.
    #[test]
    fn every_advertised_keystore_class_has_an_engine_surface() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        super::register_keystore_real(&mut registry);
        let mut missing = Vec::new();
        for fqn in ADVERTISED_KEYSTORE_CLASSES {
            // `engineLoad` stands for the whole surface: `register_engine_surface`
            // registers them together or not at all.
            if registry
                .find(fqn, "engineLoad", "(Ljava/io/InputStream;[C)V")
                .is_none()
            {
                missing.push(*fqn);
            }
        }
        assert!(
            missing.is_empty(),
            "the provider table advertises these KeyStore classes and this module \
             registers no engine surface for them, so every engineLoad on one is a \
             method with no body: {missing:?}"
        );
    }

    /// …and the list the guard walks must be the list the provider table
    /// actually publishes, or the guard checks a fiction. Compared against the
    /// SOURCE of `provider_chain`'s SUN/SunJSSE rows rather than a copy.
    #[test]
    fn the_advertised_list_matches_the_provider_table() {
        let src = include_str!("jca/provider_chain.rs");
        for fqn in ADVERTISED_KEYSTORE_CLASSES {
            let dotted = fqn.replace('/', ".");
            assert!(
                src.contains(&format!("\"{dotted}\"")),
                "{dotted} is in ADVERTISED_KEYSTORE_CLASSES but no longer appears in \
                 provider_chain.rs — drop it here, or the guard is checking a class \
                 nothing advertises"
            );
        }
    }
}

/// FQNs we register on. PKCS12 + JKS share the same `engine*` surface; we
/// register on each FQN explicitly because dispatch is keyed by class name
/// (no Java-inheritance walk on the native side — `java/security/KeyStore`
/// itself reaches us via the existing `phases_early.rs` route which now
/// delegates to this module's helpers).
const PKCS12_FQN: &str = "sun/security/pkcs12/PKCS12KeyStore";
const JKS_FQN: &str = "sun/security/provider/JavaKeyStore";
const JKS_INNER_JKS_FQN: &str = "sun/security/provider/JavaKeyStore$JKS";
const JKS_INNER_DUAL_FQN: &str = "sun/security/provider/JavaKeyStore$DualFormatJKS";
/// SunJCE's `KeyStore.JCEKS`. Its on-disk format is JKS's under a different
/// magic (`JCEKS_MAGIC`), so it shares the whole engine surface; what it does
/// NOT share is the magic `engine_store` writes, which is keyed on this name.
const JCEKS_FQN: &str = "com/sun/crypto/provider/JceKeyStore";
/// `KeyStore.getInstance("PKCS12")` — the DEFAULT, and the one netty, Tomcat
/// and every `SslContextBuilder` reach for.
///
/// Registered late, and the omission cost 191 tests. When the SUN provider's
/// `KeyStore.PKCS12` row was corrected to name the real JDK class
/// (`…$DualFormatPKCS12`, a `KeyStoreDelegator` that sniffs the stream), the
/// row started naming a class this file does not register — and dispatch here
/// is keyed by CLASS NAME with no inheritance walk, as the comment above says.
/// So the engine surface silently went missing: every `engineLoad` on the
/// platform-default PKCS#12 store did nothing, netty's key material came back
/// empty, and `JdkSslEngineTest` went 754 ok / 1 failed to 563 / 192, with
/// `IOException: setNeedClientAuth(true) requires javax.net.ssl.trustStore`
/// among the wreckage — an engine falling back to `default_engine_server_config`
/// because its `SSLContext` never got an identity.
///
/// The JKS half of that same change WAS registered (`JKS_INNER_DUAL_FQN`
/// above). Only its twin was missed, which is why
/// `every_advertised_keystore_class_has_an_engine_surface` now exists.
const PKCS12_INNER_DUAL_FQN: &str = "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12";
/// `KeyStore.getInstance("CaseExactJKS")` — advertised long before the row
/// correction and never registered either, so this one is not a regression,
/// just the same hole one door along.
const JKS_INNER_CASE_EXACT_FQN: &str = "sun/security/provider/JavaKeyStore$CaseExactJKS";
/// `KeyStore.getInstance("DKS")`. A domain keystore is a different FORMAT
/// (a policy file naming other stores), so serving it through this engine
/// surface is not right in the long run — but an unregistered class answers
/// nothing at all, and answering a `KeyStoreException` from a real
/// `engineLoad` is strictly closer to the JDK than a method with no body.
const DKS_FQN: &str = "sun/security/provider/DomainKeyStore$DKS";

const SUN_KEYSTORE_FQN: &str = "java/security/KeyStore";

const FIELD_STORE_ID: usize = 4;

/// Public entry point — wave coordinator wires this from `lib.rs`.
pub fn register_keystore_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_engine_surface(r, PKCS12_FQN);
    register_engine_surface(r, JKS_FQN);
    register_engine_surface(r, JKS_INNER_JKS_FQN);
    register_engine_surface(r, JKS_INNER_DUAL_FQN);
    register_engine_surface(r, PKCS12_INNER_DUAL_FQN);
    register_engine_surface(r, JKS_INNER_CASE_EXACT_FQN);
    register_engine_surface(r, DKS_FQN);
    register_engine_surface(r, JCEKS_FQN);

    // The `java.security.KeyStore` shim's `load`/`getKey`/`getCertificate`
    // engine surface is registered by `phases_early.rs` — we don't override
    // its registrations (forbidden surface). Instead we re-export the
    // helpers it can call into through `crate::keystore::*` once the
    // wave coordinator wires this module.

    // `engine_aliases` returns a synthetic `java/util/IteratorEnumeration`
    // (array at slot 0, position at slot 1). Its `hasMoreElements`/`nextElement`
    // were only registered in the synthetic-JDK path
    // (`phases_early::register_phase53_security`, via `register_synthetic_overrides`),
    // which real-JDK mode never calls — so in real-JDK mode the TLS
    // `KeyManagerFactory`/`TrustManagerFactory` init that walks `ks.aliases()`
    // hit `NoSuchMethodError IteratorEnumeration.hasMoreElements()`. Register
    // them here (real-JDK path), co-located with the producer.
    r.register(
        "java/util/IteratorEnumeration",
        "hasMoreElements",
        "()Z",
        |ctx, args| {
            let this = this_arg(args)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let len = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => ctx.array_length(arr),
                _ => 0,
            };
            Ok(Some(Value::Int(if pos < len { 1 } else { 0 })))
        },
    );
    r.register(
        "java/util/IteratorEnumeration",
        "nextElement",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = this_arg(args)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let elem = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => {
                    if pos < ctx.array_length(arr) {
                        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
                        ctx.get_array_element(arr, pos)
                    } else {
                        Value::Object(None)
                    }
                }
                _ => Value::Object(None),
            };
            Ok(Some(elem))
        },
    );

    let _ = SUN_KEYSTORE_FQN;
    r.set_category(__prev_cat);
}

fn register_engine_surface(r: &mut NativeMethodRegistry, fqn: &'static str) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // engineLoad(InputStream, char[])
    r.register(fqn, "engineLoad", "(Ljava/io/InputStream;[C)V", engine_load);

    // engineGetKey(String, char[]) -> Key
    r.register(
        fqn,
        "engineGetKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
        engine_get_key,
    );

    // engineGetCertificate(String) -> Certificate
    r.register(
        fqn,
        "engineGetCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        engine_get_certificate,
    );

    // engineGetCertificateChain(String) -> Certificate[]
    r.register(
        fqn,
        "engineGetCertificateChain",
        "(Ljava/lang/String;)[Ljava/security/cert/Certificate;",
        engine_get_certificate_chain,
    );

    // engineAliases() -> Enumeration<String>
    r.register(
        fqn,
        "engineAliases",
        "()Ljava/util/Enumeration;",
        engine_aliases,
    );

    // engineSize() -> int
    r.register(fqn, "engineSize", "()I", engine_size);
    // The seventeenth `engine*`. Missing, it did not degrade -- it reached
    // real `KeyStoreDelegator` bytecode and raised NPE on a field this module
    // never populates. See `engine_get_certificate_alias`.
    r.register(
        fqn,
        "engineGetCertificateAlias",
        "(Ljava/security/cert/Certificate;)Ljava/lang/String;",
        engine_get_certificate_alias,
    );

    // engineContainsAlias(String) -> boolean
    r.register(
        fqn,
        "engineContainsAlias",
        "(Ljava/lang/String;)Z",
        engine_contains_alias,
    );

    // engineIsKeyEntry(String) -> boolean
    r.register(
        fqn,
        "engineIsKeyEntry",
        "(Ljava/lang/String;)Z",
        engine_is_key_entry,
    );

    // engineIsCertificateEntry(String) -> boolean
    r.register(
        fqn,
        "engineIsCertificateEntry",
        "(Ljava/lang/String;)Z",
        engine_is_certificate_entry,
    );

    // engineGetCreationDate(String) -> Date
    r.register(
        fqn,
        "engineGetCreationDate",
        "(Ljava/lang/String;)Ljava/util/Date;",
        engine_get_creation_date,
    );

    // engineSetCertificateEntry(String, Certificate) — in-memory mutation,
    // writes the same side-table the read natives consult (the real bytecode
    // updated a separate `entries` field invisible to engineAliases).
    r.register(
        fqn,
        "engineSetCertificateEntry",
        "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
        engine_set_certificate_entry,
    );

    // engineSetKeyEntry(String, Key, char[], Certificate[]) — companion to
    // engineSetCertificateEntry above for the PrivateKey case. See
    // keystore_set_key_entry's doc comment: without this, KeyManagerFactory
    // found no staged identity for an in-memory-only keystore built via
    // getInstance()+load(null,null)+setKeyEntry(...) (Netty's
    // JdkSslServerContext.buildKeyStore ephemeral self-signed-cert pattern),
    // and SSLContext.init produced a server-side SSLContext with no
    // certificate to present — the TLS handshake then failed immediately
    // ("unexpected EOF" on the client side). See
    // docs/known-issues/http-server-cluster-residuals.md.
    r.register(
        fqn,
        "engineSetKeyEntry",
        "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
        engine_set_key_entry,
    );
    // engineDeleteEntry(String) — companion in-memory removal.
    r.register(
        fqn,
        "engineDeleteEntry",
        "(Ljava/lang/String;)V",
        engine_delete_entry,
    );

    // engineStore(OutputStream, char[]) — serialise the side-table (as JKS,
    // which CV's load path detects by magic) so a store→load round-trip
    // preserves entries.
    r.register(
        fqn,
        "engineStore",
        "(Ljava/io/OutputStream;[C)V",
        engine_store,
    );

    // engineSetEntry / engineGetEntry — the `KeyStore.Entry`-shaped half of the
    // API. See `engine_set_entry`'s doc comment for why leaving these to real
    // bytecode was a silent data loss rather than merely a gap.
    r.register(
        fqn,
        "engineSetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$Entry;Ljava/security/KeyStore$ProtectionParameter;)V",
        engine_set_entry,
    );
    r.register(
        fqn,
        "engineGetEntry",
        "(Ljava/lang/String;Ljava/security/KeyStore$ProtectionParameter;)Ljava/security/KeyStore$Entry;",
        engine_get_entry,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// `engine*` callback implementations
// ---------------------------------------------------------------------------

/// The alias, as the JDK's own keystore SPIs key it: **NPE on null, lowercased
/// otherwise**.
///
/// Both halves are measured, in both modes
/// (`probes/KeyStoreFamilySweep.java`, HotSpot 25.0.3+9):
///
/// ```text
/// setCertificateEntry("MixedCaseAlias", c); aliases()
///   HotSpot  [mixedcasealias]      CratonVM  [MixedCaseAlias]
/// containsAlias("mixedcasealias")
///   HotSpot  true                  CratonVM  false
/// containsAlias(null)
///   HotSpot  NullPointerException  CratonVM  no-throw
/// ```
///
/// `PKCS12KeyStore` and `JavaKeyStore` both do `alias.toLowerCase(Locale
/// .ENGLISH)` before touching their map, which is where both behaviours come
/// from at once: the null dereference and the case folding. This VM read the
/// alias with `.unwrap_or_default()`, so a null alias silently became `""` and
/// a mixed-case one stayed mixed.
///
/// Why the case matters beyond a probe row: aliases are how a keystore file
/// written by `keytool` is addressed. A store written elsewhere and read here
/// (or the reverse) disagreed about every alias that was not already lower
/// case, and `containsAlias` answered false for a name the store really held.
///
/// `to_lowercase` rather than `to_ascii_lowercase`: Rust's is locale-INDEPENDENT
/// Unicode lowercasing, which is what `Locale.ENGLISH` selects; the ASCII form
/// would diverge on a non-ASCII alias, and the locale-sensitive form is the
/// Turkish-dotless-I trap the JDK pins `Locale.ENGLISH` to avoid.
fn alias_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> Result<String, MethodCallFailed> {
    match args.get(1) {
        Some(v @ Value::Object(Some(_))) => match read_string_arg(ctx, v) {
            Some(a) => Ok(a.to_lowercase()),
            // A non-null object that is not a readable String: keep the old
            // lenient behaviour rather than inventing a second failure mode.
            None => Ok(String::new()),
        },
        _ => Err(RuntimeError::NullPointerException {
            message: Some("KeyStore alias is null".into()),
        }
        .into()),
    }
}

fn this_arg(args: &[Value]) -> Result<ObjectRef, MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(r))) => Ok(*r),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("KeyStore engine call on null receiver".into()),
        }
        .into()),
    }
}

pub(crate) fn read_password(ctx: &mut dyn NativeContext, v: &Value) -> Vec<u8> {
    if let Value::Object(Some(arr)) = v {
        let len = ctx.array_length(*arr);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            // char[] in our model is Value::Int holding the 16-bit codepoint.
            // We project to 0..=0xFF (Latin-1) for the JKS path; PKCS#12
            // path uses the str roundtrip below. Anything > 0xFF gets its
            // low byte; passwords with non-Latin-1 chars are exotic.
            if let Value::Int(c) = ctx.get_array_element(*arr, i) {
                out.push((c & 0xFF) as u8);
            }
        }
        out
    } else {
        Vec::new()
    }
}

/// Pull every byte the `InputStream` will give us until EOF.
///
/// We try the cheap path first: if the stream is a `ByteArrayInputStream`
/// (the common case in real-JDK keystore loading — the JKS code wraps the
/// input bytes in BAIS internally, and most callers also pass a BAIS), we
/// can skip the bytecode-level read loop and pull bytes straight out of
/// the backing array. Field layout: `buf=0, pos=1, mark=2, count=3`.
///
/// If that doesn't apply, fall back to invoking
/// `InputStream.read(byte[], int, int)` in a loop. This is slower because
/// each invocation is a full bytecode trip, but it's the only correct path
/// for arbitrary streams (FileInputStream, network streams, gzip, ...).
fn read_stream_to_end(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Vec<u8> {
    // Cheap path: ByteArrayInputStream
    if matches!(ctx.heap_kind_of(stream), cratonvm_types::ObjectKind::Object) {
        let cls_id = ctx.class_id_of_object(stream);
        if let Some(name) = ctx.class_name_of_id(cls_id) {
            if name == "java/io/ByteArrayInputStream" {
                let buf = ctx.get_field(stream, 0);
                let pos = match ctx.get_field(stream, 1) {
                    Value::Int(v) => v as usize,
                    _ => 0,
                };
                let count = match ctx.get_field(stream, 3) {
                    Value::Int(v) => v as usize,
                    _ => 0,
                };
                if let Value::Object(Some(arr)) = buf {
                    let total = ctx.array_length(arr).min(count);
                    let start = pos.min(total);
                    let mut out = Vec::with_capacity(total - start);
                    for i in start..total {
                        if let Value::Int(b) = ctx.get_array_element(arr, i) {
                            out.push(b as u8);
                        }
                    }
                    return out;
                }
            }
        }
    }

    // General path: invoke `int read(byte[], int, int)` in a loop.
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    let chunk_size = 4096usize;
    let chunk = ctx.new_array(ArrayElementType::Byte, chunk_size);
    let chunk_pin = ctx.pin_native_root(chunk);
    let stream_pin = ctx.pin_native_root(stream);
    loop {
        let chunk = ctx.read_native_pin(chunk_pin, chunk);
        let stream = ctx.read_native_pin(stream_pin, stream);
        let res = ctx.invoke(
            "java/io/InputStream",
            "read",
            "([BII)I",
            &[
                Value::Object(Some(stream)),
                Value::Object(Some(chunk)),
                Value::Int(0),
                Value::Int(chunk_size as i32),
            ],
        );
        let n = match res {
            Ok(Some(Value::Int(n))) => n,
            // Any error or surprising return: stop trying. We may have
            // partial data; downstream parsers will detect a Truncated error
            // and surface it.
            _ => break,
        };
        if n <= 0 {
            break;
        }
        for i in 0..(n as usize) {
            if let Value::Int(b) = ctx.get_array_element(chunk, i) {
                out.push(b as u8);
            }
        }
    }
    out
}

pub(crate) fn engine_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;

    // null InputStream is "create empty". Real-JDK does the same.
    let stream_opt = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // A Java `null` char[] (as opposed to a present-but-empty one) means "no
    // password supplied" and must disable PKCS#12 MAC/integrity checking —
    // see `load_pkcs12_ex`. Distinguish the two here, since `read_password`
    // collapses both to an empty Vec (correct for the entry-decryption call
    // sites that use it, which don't care about the distinction).
    let password_arg = args.get(2);
    let password_present = !matches!(password_arg, None | Some(Value::Object(None)));
    let password = match password_arg {
        Some(v) => read_password(ctx, v),
        None => Vec::new(),
    };

    let bytes = match stream_opt {
        Some(s) => read_stream_to_end(ctx, s),
        None => Vec::new(),
    };

    let store = if bytes.is_empty() {
        LoadedKeyStore::default()
    } else {
        match load_keystore_ex(&bytes, &password, password_present) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "keystore", "engineLoad: parse failed: {e}");
                // Match real-JDK: throw IOException. For simplicity here we
                // surface a runtime error — callers (real-JDK glue) wrap
                // this as IOException at the bytecode boundary.
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException {
                        message: e.to_string(),
                    }),
                ));
            }
        }
    };

    // Bridge the parsed keystore into the rustls-backed TLS engine: install the
    // first key entry as the server identity.
    //
    // FIX (es-restclient-https): trust anchors ARE now also mirrored here (see
    // below), reversing the previous "TrustManagerFactory/SSLContext scope
    // them instead" assumption. That assumption relied on
    // `TrustManagerFactory.init(KeyStore)`'s native firing reliably — mirroring
    // the same (correct, see below) assumption made for `KeyManagerFactory
    // .init` — but empirically, `TrustManagerFactory.getInstance("PKIX")`
    // (the JDK default algorithm) resolves to
    // `sun.security.ssl.TrustManagerFactoryImpl$PKIXFactory`, whose
    // `engineInit(KeyStore)` is REAL, non-native bytecode (inherited from the
    // abstract `TrustManagerFactoryImpl.engineInit`) that CratonVM does not
    // intercept — only the SunX509 `$SimpleFactory` variant has a registered
    // native (`x509_manager.rs::tmf_engine_init`). So for the (common) PKIX
    // default algorithm, no native ever runs to call
    // `set_pending_tm_trust_roots`, and a caller-supplied truststore's anchors
    // were silently dropped: `SSLContext.init` always fell back to the
    // platform root store only, so a self-signed/test-CA certificate (e.g.
    // the `HttpsServer`+`RestClient` pattern in `RestClientBuilderIntegTests`)
    // always failed handshake with `UnknownIssuer`. `engineLoad` is the one
    // reliable native call point regardless of which `TrustManagerFactory`
    // algorithm/impl class ends up being used, so stage the anchors here too.
    let mut first_key_identity: Option<(Vec<u8>, Vec<Vec<u8>>)> = None;
    let mut trust_anchor_ders: Vec<Vec<u8>> = Vec::new();
    for entry in store.entries.values() {
        match &entry.kind {
            EntryKind::PrivateKey { key_der, chain } => {
                if !is_jks_encrypted_private_key(key_der) {
                    crate::t27_tls::install_identity_from_der(key_der, chain);
                    if first_key_identity.is_none() {
                        first_key_identity = Some((key_der.clone(), chain.clone()));
                    }
                }
                // The chain's root (last cert) is a trust anchor too — a
                // keystore holding a self-signed identity (the common test
                // pattern: `keytool -genkeypair`) IS its own trust anchor.
                if let Some(root) = chain.last() {
                    trust_anchor_ders.push(root.clone());
                }
            }
            EntryKind::TrustedCert { cert_der } => {
                trust_anchor_ders.push(cert_der.clone());
            }
            EntryKind::SecretKey { .. } => {}
        }
    }

    let id = keystore_register(store);
    set_store_id(ctx, this, id);
    // Record this keystore's identity PEM keyed by store_id, AND stage it on the
    // current thread for the next `SSLContext.init`. In real-JDK mode the
    // `KeyManagerFactory.init` natives are synthetic-gated (the real bytecode
    // runs), so `engineLoad` — which IS a real-mode native — is the reliable
    // capture point: a keystore is loaded immediately before its KMF/SSLContext
    // is built on the same thread (e.g. TesterSupport.getUserKeyManagers →
    // SSLContext.init). `SSLContext.init` consumes (clears) the thread-local, so
    // it only sticks to the very next context built after this load.
    if let Some((key_der, chain)) = first_key_identity {
        let (cert_pem, key_pem) = crate::t27_tls::der_identity_to_pem(&key_der, &chain);
        store_identity_pem_map()
            .lock()
            .unwrap()
            .insert(id, (cert_pem.clone(), key_pem.clone()));
        crate::t27_tls::set_pending_km_identity(cert_pem, key_pem);
    }
    // FIX (es-restclient-https): stage this keystore's trust anchors for the
    // next `SSLContext.init`, same lifetime/consumption rules as the KM
    // identity above (one-shot thread-local, cleared by `SSLContext.init`).
    if !trust_anchor_ders.is_empty() {
        crate::t27_tls::set_pending_tm_trust_roots(trust_anchor_ders);
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn engine_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let password = args
        .get(2)
        .map(|v| read_password(ctx, v))
        .unwrap_or_default();
    // A JKS store may be loaded without its per-entry key password (Tomcat's
    // `Ssl` configuration supplies that password later through getKey). Unlock
    // the entry at the API boundary that actually receives it, so consumers
    // never receive an EncryptedPrivateKeyInfo masquerading as PKCS#8.
    keystore_unlock_private_keys(id, &password);
    let alias = alias_arg(ctx, args)?;
    let Some(store) = keystore_lookup(id) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(entry) = store.entries.get(&alias) else {
        return Ok(Some(Value::Object(None)));
    };

    if let EntryKind::PrivateKey { key_der, .. } = &entry.kind {
        // Allocate the synthetic PrivateKey mirror. Field layout matches the
        // existing convention (algo_idx=0, key_size_bits=1, key_len_bytes=2,
        // key_id=3) so the TLS path keeps working. We additionally stash the
        // store_id + alias hash in a registry so the TLS layer can pull the
        // DER through `keystore_get_private_key()` rather than needing to
        // round-trip through this object.
        let pk = try_alloc_concurrent_synthetic(ctx, "java/security/PrivateKey", 4)?;
        let algo_idx = detect_algo_idx(key_der);
        ctx.set_field(pk, 0, Value::Int(algo_idx));
        ctx.set_field(pk, 1, Value::Int((key_der.len() as i32).saturating_mul(8)));
        ctx.set_field(pk, 2, Value::Int(key_der.len() as i32));
        // store_id encoded into the high 32 bits, alias-hash in the low 32.
        let alias_hash = fnv1a_32(alias.as_bytes());
        let composite = ((id as i64 & 0xFFFF_FFFF) << 32) | (alias_hash as i64 & 0xFFFF_FFFF);
        ctx.set_field(pk, 3, Value::Long(composite));
        // FIX (sslWithPemCertificates-decrypterror-20260720): `jca::signature`'s
        // `extract_key_id_from_key` reads THIS SAME field slot 3 as a
        // `crypto_impl` RSA key_id when the key's `identityHashCode` isn't
        // found in `rsa_realkey_map` first — but slot 3 here is the
        // (store_id, alias_hash) composite above, a completely different
        // namespace. `Signature.sign()` on a `KeyStore.getKey()`-sourced
        // PrivateKey therefore signed with whatever unrelated key happened to
        // occupy that same numeric id in `crypto_impl`'s RSA_KEY_STORE (or
        // silently produced a bad signature), causing rustls's TLS 1.3
        // CertificateVerify check to fail on the peer with `BadSignature` /
        // `DecryptError` during mTLS — reproduced in isolation (no TLS
        // involved) by round-tripping `Signature.sign()`/`verify()` on a
        // `KeyStore.getKey()`-sourced PKCS12 RSA key: verify failed on
        // CratonVM, succeeded on HotSpot, with byte-identical key material.
        // Register the real key material under this object's identity hash —
        // exactly like `register_rsa_priv_sign_material` does for
        // `KeyFactory.generatePrivate` imports — so `extract_key_id_from_key`
        // finds the correct key_id before ever falling through to slot 3.
        if algo_idx == 6 {
            if let Some(kp) = crypto_impl::parse_rsa_private_key_pkcs8(key_der) {
                let key_id = crypto_impl::rsa_key_next_id();
                crypto_impl::rsa_key_store(key_id, kp);
                // VM-scoped key -- see `crypto_impl::RSA_REALKEY_MAP`'s doc
                // comment: an identity hash is unique only within one heap and
                // the map is a process-global static.
                crypto_impl::rsa_realkey_map_set(
                    ctx.vm_identity(),
                    ctx.identity_hash_code(pk),
                    key_id,
                );
            }
        }
        Ok(Some(Value::Object(Some(pk))))
    } else if let EntryKind::SecretKey {
        key_bytes,
        algorithm,
    } = &entry.kind
    {
        // Return the concrete mirror rather than the `SecretKey` interface:
        // SmallRye asks `Key.getEncoded()`, whose real interface method has no
        // code body. `SecretKeySpec` has registered accessors and preserves
        // the raw key bytes in field 0.
        let key = try_alloc_concurrent_synthetic(ctx, "javax/crypto/spec/SecretKeySpec", 2)?;
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, key_bytes.len());
        for (i, byte) in key_bytes.iter().enumerate() {
            ctx.set_array_element(bytes, i, Value::Int(*byte as i8 as i32));
        }
        ctx.set_field(key, 0, Value::Object(Some(bytes)));
        // The entry's OWN algorithm, not the constant "RAW" that used to be
        // written here: "RAW" is `SecretKeySpec.getFormat()`, and reporting the
        // format as the ALGORITHM broke every
        // `Cipher.getInstance(key.getAlgorithm())` on a key recovered from a
        // keystore.
        let alg_name = ctx.create_string(algorithm);
        ctx.set_field(key, 1, Value::Object(Some(alg_name)));
        Ok(Some(Value::Object(Some(key))))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// Return the DER behind the compact four-slot `PrivateKey` proxy emitted by
/// [`engine_get_key`]. The proxy's final slot is a `(store_id, alias_hash)`
/// handle, not a fifth in-object DER field; `Key.getEncoded()` must resolve it
/// through the keystore registry instead of reading past the real interface
/// object's layout.
pub(crate) fn private_key_der_from_proxy(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
) -> Option<Vec<u8>> {
    if ctx.object_num_fields(key) <= 3 {
        return None;
    }
    let composite = match ctx.get_field(key, 3) {
        Value::Long(value) => value,
        _ => return None,
    };
    let alias_hash = composite as u32;
    // The other producer of this layout is
    // `x509_manager::make_private_key_mirror`, whose high half is a
    // `KeyManager` id from a DIFFERENT counter. It tags itself so the two are
    // distinguishable; untagged composites are keystore ids as before. Reading
    // a tagged one as a store id is what made
    // `KeyManager.getPrivateKey(alias).getEncoded()` return an empty array
    // whenever the two counters were not coincidentally aligned.
    if crate::x509_manager::is_km_proxy_composite(composite) {
        let km_id = crate::x509_manager::km_id_of_composite(composite);
        if km_id <= 0 {
            return None;
        }
        return crate::x509_manager::km_key_der_by_alias_hash(km_id, alias_hash);
    }
    let store_id = (composite >> 32) as i32;
    if store_id <= 0 {
        return None;
    }
    let store = keystore_lookup(store_id)?;
    store.entries.values().find_map(|entry| {
        if fnv1a_32(entry.alias.as_bytes()) != alias_hash {
            return None;
        }
        match &entry.kind {
            EntryKind::PrivateKey { key_der, .. } => Some(key_der.clone()),
            _ => None,
        }
    })
}

pub(crate) fn engine_get_certificate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    let Some(store) = keystore_lookup(id) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(entry) = store.entries.get(&alias) else {
        return Ok(Some(Value::Object(None)));
    };

    let cert_der = match &entry.kind {
        EntryKind::TrustedCert { cert_der } => cert_der.clone(),
        EntryKind::PrivateKey { chain, .. } => match chain.first() {
            Some(d) => d.clone(),
            None => return Ok(Some(Value::Object(None))),
        },
        EntryKind::SecretKey { .. } => return Ok(Some(Value::Object(None))),
    };

    Ok(Some(Value::Object(Some(make_x509_mirror(
        ctx, &alias, &cert_der,
    )?))))
}

pub(crate) fn engine_get_certificate_chain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    let Some(store) = keystore_lookup(id) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(entry) = store.entries.get(&alias) else {
        return Ok(Some(Value::Object(None)));
    };

    let chain = match &entry.kind {
        EntryKind::PrivateKey { chain, .. } => chain.clone(),
        EntryKind::TrustedCert { .. } => return Ok(Some(Value::Object(None))),
        EntryKind::SecretKey { .. } => return Ok(Some(Value::Object(None))),
    };

    let cls_id = match ctx.ensure_class_initialized("java/security/cert/X509Certificate") {
        Ok(c) => c,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): this is the
        // element class of the returned `Certificate[]`, and a fabricated
        // stand-in for a `java.security.cert` class is the compatibility
        // substitution contract §5 refuses. On a complete image the `Ok` arm is
        // what runs, so this changes nothing outside `--jdk-only` on a broken
        // image.
        Err(_) => {
            crate::util_concurrent_ext::refused_class(ctx, "java/security/cert/X509Certificate", 8)?
        }
    };
    let arr =
        crate::util_concurrent_ext::build_rooted_ref_array(ctx, cls_id, chain.len(), |ctx, i| {
            make_x509_mirror(ctx, &alias, &chain[i])
        })?;
    Ok(Some(Value::Object(Some(arr))))
}

pub(crate) fn engine_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);

    let store = keystore_lookup(id).unwrap_or_default();
    // Insertion order (`entries` is an `IndexMap`), NOT alphabetical — real
    // JDK's `KeyStore.aliases()` enumerates in the order entries were read
    // from the file (`LinkedHashMap`-backed), and Spring Boot's `SslInfo`
    // asserts on that exact positional order (see `LoadedKeyStore::entries`'s
    // doc comment).
    let aliases: Vec<String> = store.entries.keys().cloned().collect();

    let cls_id = match ctx.ensure_class_initialized("java/lang/String") {
        Ok(c) => c,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). `java.lang.String`
        // is in every image, so the `Ok` arm is what runs; a run that reaches
        // this one has no `java.base` at all and fabricating a `String` stand-in
        // is not a recovery, it is a second failure wearing the first one's name.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/String", 8)?,
    };
    let arr = crate::util_concurrent_ext::build_rooted_ref_array(
        ctx,
        cls_id,
        aliases.len(),
        |ctx, i| Ok(ctx.create_string(&aliases[i])),
    )?;

    // Under `--jdk-only` this fabrication is refused; fall back to an
    // enumeration the JDK builds itself (`Arrays$ArrayList` +
    // `Collections.enumeration`), the same landing `Enumeration$Impl` uses.
    // Without it the refusal surfaces as NoClassDefFoundError out of
    // `KeyStore.aliases()` and takes the whole `security` section of
    // `JdkOnlyPlatformProbe` with it. `Compatible` mode is unchanged: the
    // fallback is only reachable from the refusal arm.
    let arr_pin = ctx.pin_native_root(arr);
    let en = match try_alloc_concurrent_synthetic(ctx, "java/util/IteratorEnumeration", 2) {
        Ok(en) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(en, 0, Value::Object(Some(arr)));
            ctx.set_field(en, 1, Value::Int(0));
            Ok(en)
        }
        Err(refusal) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            match crate::classloader::real_snapshot_enumeration(ctx, arr) {
                Ok(Some(en)) => Ok(en),
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(arr_pin);
    Ok(Some(Value::Object(Some(en?))))
}

/// Public `KeyStore.aliases()` is intercepted by the early security shim in
/// real-JDK mode. Route that wrapper through the provider SPI's registry-backed
/// implementation rather than returning the legacy synthetic empty view.
pub(crate) fn keystore_aliases(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine_aliases(ctx, &engine_args)
}

pub(crate) fn engine_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let n = keystore_lookup(id).map(|s| s.entries.len()).unwrap_or(0);
    Ok(Some(Value::Int(n as i32)))
}

/// Registry-backed counterpart for the public `KeyStore.size()` shim.
pub(crate) fn keystore_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine_size(ctx, &engine_args)
}

pub(crate) fn engine_contains_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let present = keystore_lookup(id)
        .map(|s| s.entries.contains_key(&alias))
        .unwrap_or(false);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

pub(crate) fn engine_is_key_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let yes = keystore_lookup(id)
        .and_then(|s| {
            s.entries.get(&alias).map(|e| {
                matches!(
                    e.kind,
                    EntryKind::PrivateKey { .. } | EntryKind::SecretKey { .. }
                )
            })
        })
        .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

pub(crate) fn engine_is_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let yes = keystore_lookup(id)
        .and_then(|s| {
            s.entries
                .get(&alias)
                .map(|e| matches!(e.kind, EntryKind::TrustedCert { .. }))
        })
        .unwrap_or(false);
    Ok(Some(Value::Int(if yes { 1 } else { 0 })))
}

fn engine_get_creation_date(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    // An ABSENT alias is null, not the epoch. `.unwrap_or(0)` handed back a
    // `Date` at time 0, so a caller testing `getCreationDate(a) != null` to ask
    // "does this entry exist" got true for every alias in existence.
    //
    // MEASURED in both modes (`probes/KeyStoreFamilySweep.java`):
    //
    //   getCreationDate("nope")
    //     HotSpot   null
    //     CratonVM  Wed Dec 31 21:00:00 UTC 1969
    //
    // Note the rendered date is the LOCAL-time face of epoch 0, which is what
    // made it read like a real timestamp rather than a default.
    let Some(ms) =
        keystore_lookup(id).and_then(|s| s.entries.get(&alias).map(|e| e.creation_time_ms))
    else {
        return Ok(Some(Value::Object(None)));
    };

    // java/util/Date has a single `fastTime` long field in real-JDK layout
    // (slot 0 in our synthetic mirror).
    let date = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
    ctx.set_field(date, 0, Value::Long(ms));
    Ok(Some(Value::Object(Some(date))))
}

/// engineSetCertificateEntry(String alias, Certificate cert) — in-memory
/// mutation. The read-side natives (engineAliases/engineSize/engineGetCertificate)
/// are backed by the CV keystore side-table, but the real PKCS12KeyStore
/// bytecode for setCertificateEntry updates its own `entries` field, which the
/// natives never read — so `setCertificateEntry` was invisible to `aliases()`
/// (keycloak TruststoreBuilder: merged truststore reported 0 entries). Intercept
/// it and write the same side-table the reads consult. The cert DER comes from
/// [`certificate_der`] (real `Certificate.getEncoded()`, with a fallback for
/// this module's own synthetic X.509 mirror).
/// `KeyStoreSpi.engineGetCertificateAlias(Certificate)` — the first alias whose
/// stored certificate has the SAME ENCODING as the argument, or null.
///
/// MEASURED 2026-08-27 (`probes/KeyStoreFamilySweep.java`), both modes, and it
/// was FATAL rather than merely wrong:
///
/// ```text
/// KeyStore.getCertificateAlias(cert)
///   HotSpot   "mixedcasealias"
///   CratonVM  NullPointerException: Cannot invoke
///             "java.security.KeyStoreSpi.engineGetCertificateAlias(..)"
///             because "this.keystore" is null
///             at sun/security/util/KeyStoreDelegator.engineGetCertificateAlias
/// ```
///
/// The probe died at row 33 of 160, so 128 rows never ran.
///
/// Why it happened, which is the reusable part: this module registers SIXTEEN
/// `engine*` methods and this was the seventeenth. An unregistered one does not
/// fall back to something harmless — it reaches the REAL `KeyStoreDelegator`
/// bytecode, which dereferences `this.keystore`, a field the natives never
/// populate because this module keeps its state in its own side table. So the
/// cost of a missing registration in a half-shimmed class is a
/// `NullPointerException` from inside the JDK, not a missing feature. That is
/// the shape recorded at `a-shim-registered-for-a-descriptor-it-half-implements`.
///
/// Comparison is by DER, not by mirror identity: the argument is whatever
/// `Certificate` the caller holds, which in general is a different object from
/// the one `engineGetCertificate` would mint for the same entry.
pub(crate) fn engine_get_certificate_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let Some(Value::Object(Some(cert))) = args.get(1) else {
        // The JDK's own body answers null for a null certificate rather than
        // throwing: `engineGetCertificateAlias` has no null check and the scan
        // simply matches nothing.
        return Ok(Some(Value::Object(None)));
    };
    let want = certificate_der(ctx, *cert);
    if want.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let Some(store) = keystore_lookup(id) else {
        return Ok(Some(Value::Object(None)));
    };
    // `entries` is a BTreeMap, so the scan order is deterministic; the JDK's
    // own order is its hash table's and is unspecified, which is why the probe
    // only ever puts ONE certificate in the store before asking.
    for (alias, entry) in &store.entries {
        let have = match &entry.kind {
            EntryKind::TrustedCert { cert_der } => cert_der.clone(),
            EntryKind::PrivateKey { chain, .. } => match chain.first() {
                Some(d) => d.clone(),
                None => continue,
            },
            EntryKind::SecretKey { .. } => continue,
        };
        if have == want {
            let alias = alias.clone();
            return Ok(Some(Value::Object(Some(ctx.create_string(&alias)))));
        }
    }
    Ok(Some(Value::Object(None)))
}

pub(crate) fn engine_set_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let cert = match args.get(2) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    // Pull the DER via the real Certificate.getEncoded() (with the synthetic
    // mirror fallback — see `certificate_der`).
    let der = certificate_der(ctx, cert);
    if der.is_empty() {
        // No encoding available — nothing to store (lenient; real JDK would
        // throw KeyStoreException, but a valid Certificate always encodes).
        return Ok(None);
    }
    keystore_set_cert_entry(id, &alias, der);
    Ok(None)
}

/// engineSetKeyEntry(String alias, Key key, char[] password, Certificate[]
/// chain) -- in-memory PrivateKey entry (companion to
/// engine_set_certificate_entry; see keystore_set_key_entry's doc comment
/// for why this matters). The key's PKCS#8 DER comes from the real
/// `PrivateKey.getEncoded()` (falling back to [`private_key_der_from_proxy`]
/// for this module's compact four-slot key proxy); each chain cert's DER from
/// [`certificate_der`], the same extraction engine_set_certificate_entry uses.
pub(crate) fn engine_set_key_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    let key = match args.get(2) {
        Some(Value::Object(Some(k))) => *k,
        _ => return Ok(None),
    };

    // `engineSetKeyEntry(String, Key, char[], Certificate[])` accepts ANY
    // `Key`. Real `PKCS12KeyStore` branches on `instanceof PrivateKey` vs
    // `instanceof SecretKey` and writes a SecretBag for the latter; this native
    // used to store every key as a `PrivateKey` entry, so
    // `setKeyEntry(a, new SecretKeySpec(raw, "DESede"), pw, null)` came back
    // out of `getKey` as a `PrivateKey` claiming algorithm RSA -- the raw bytes
    // survived, their type and algorithm did not.
    //
    // `Key.getFormat()` is the spec-defined discriminator ("RAW" for a secret
    // key, "PKCS#8" for a private key) and costs one virtual call, so ask it
    // rather than plumbing interface-identity checks through the native API.
    if string_from_virtual(ctx, key, "getFormat", "()Ljava/lang/String;").as_deref() == Some("RAW")
    {
        if !spi_supports_secret_keys(ctx, this) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/KeyStoreException",
                "Cannot store non-PrivateKeys",
            ));
        }
        let raw = read_encoded_byte_array(ctx, key);
        if raw.is_empty() {
            return Ok(None);
        }
        let key_algorithm = string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;")
            .unwrap_or_else(|| "AES".to_string());
        if let Err(failure) = require_encodable_secret_alg(ctx, &key_algorithm) {
            return Err(failure);
        }
        keystore_set_secret_key_entry(id, &alias, raw, &key_algorithm);
        return Ok(None);
    }

    let mut key_der = read_encoded_byte_array(ctx, key);
    if key_der.is_empty() {
        // `engine_get_key`'s compact four-slot `PrivateKey` proxy carries a
        // `(store_id, alias_hash)` handle instead of an in-object DER; when
        // `java/security/PrivateKey.getEncoded` is not the jca::key_factory
        // shim (which resolves that handle itself) the virtual call yields
        // nothing. Resolve the handle directly before giving up — otherwise
        // `ks2.setKeyEntry(a, ks1.getKey(a, pw), pw, ks1.getCertificateChain(a))`
        // (the standard keystore-merge idiom) silently dropped the entry.
        key_der = private_key_der_from_proxy(ctx, key).unwrap_or_default();
    }
    if key_der.is_empty() {
        // No PKCS#8 encoding available (e.g. a PKCS#11/HSM-backed key with
        // getEncoded() == null) -- nothing we can stage natively. Lenient,
        // matches the same leniency engine_set_certificate_entry takes.
        //
        // But not SILENT: an opaque key is the normal case for a delegating
        // provider (`MockAlternativeKeyProvider` in netty's
        // `JdkDelegatingPrivateKeyMethodTest`, any PKCS#11 or HSM key), and
        // dropping its entry with no trace means the alias simply is not there
        // later — indistinguishable from a keystore that was never populated.
        // the openssl-key-material-and-engine-residuals write-up (now retired)
        // asked for exactly this line. The store keeps working through the
        // live-keystore enumeration path
        // (`x509_manager::build_key_manager_state_from_live_keystore`), which
        // holds such a key BY REFERENCE, so this is a note about the NATIVE
        // staging only, not necessarily a broken handshake.
        eprintln!(
            "[keystore] setKeyEntry store_id={id} alias={alias:?} NOT staged natively: the key              reports no encoding (getEncoded() == null / empty), which is normal for an opaque              provider key. algorithm={:?} format={:?}; the live-KeyStore enumeration path keeps              it by reference instead.",
            string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;"),
            string_from_virtual(ctx, key, "getFormat", "()Ljava/lang/String;"),
        );
        return Ok(None);
    }
    let mut chain: Vec<Vec<u8>> = Vec::new();
    if let Some(Value::Object(Some(chain_arr))) = args.get(4) {
        let len = ctx.array_length(*chain_arr);
        for i in 0..len {
            if let Value::Object(Some(cert)) = ctx.get_array_element(*chain_arr, i) {
                let cv = certificate_der(ctx, cert);
                if !cv.is_empty() {
                    chain.push(cv);
                }
            }
        }
    }
    // FIX (httpserver-pkcs12-20260706): also install this as the process-wide
    // "runtime server identity" the native TLS listener consumes for the
    // actual accept-side handshake (io_native_tls's rustls ServerConfig),
    // exactly like engineLoad does for the byte-stream-load case. Without
    // this, a keystore built purely in-memory (getInstance() + load(null,
    // null) + setKeyEntry(...) -- Netty's JdkSslServerContext.buildKeyStore
    // ephemeral self-signed-cert pattern) staged an identity for
    // KeyManagerFactory/SSLContext's per-context bookkeeping (see
    // keystore_set_pending_km_identity's live-scan fallback above) but the
    // actual server socket had no certificate to present at all, and every
    // TLS handshake against it failed immediately ("unexpected EOF" on the
    // client side). See docs/known-issues/http-server-cluster-residuals.md.
    if crate::nbflags().dbg_tls_hs {
        eprintln!(
            "[dbg-tls-hs] engine_set_key_entry store_id={} alias={:?} key_len={} chain_len={} chain_cert_lens={:?}",
            id,
            alias,
            key_der.len(),
            chain.len(),
            chain.iter().map(|c| c.len()).collect::<Vec<_>>()
        );
    }
    if !chain.is_empty() {
        if crate::nbflags().dbg_tls_hs {
            eprintln!("[dbg-tls-hs] install_identity_from_der CALLER=engine_set_key_entry(direct-API) key_len={}", key_der.len());
        }
        crate::t27_tls::install_identity_from_der(&key_der, &chain);
    }
    keystore_set_key_entry(id, &alias, key_der, chain);
    Ok(None)
}

/// Refuse a secret-key algorithm this VM cannot encode into a SecretBag,
/// AT THE POINT THE ENTRY IS SET.
///
/// Real `PKCS12KeyStore.setKeyEntry` calls `AlgorithmId.get(algorithm)` while
/// storing and wraps its `NoSuchAlgorithmException` in a `KeyStoreException`,
/// so `setEntry` with e.g. a `"ChaCha20"` `SecretKeySpec` fails immediately on
/// HotSpot too. Deferring the complaint to `store()` would leave a caller
/// holding a keystore that looks fine and cannot be written — a second silent
/// no-op in place of the one this record is about.
fn require_encodable_secret_alg(
    ctx: &mut dyn NativeContext,
    algorithm: &str,
) -> Result<(), MethodCallFailed> {
    if secret_key_alg_oid(algorithm).is_some() {
        return Ok(());
    }
    Err(crate::phases_early::throw_jca_exc(
        ctx,
        "java/security/KeyStoreException",
        &format!(
            "Key protection algorithm not found: no PKCS#12 object identifier is known for              secret-key algorithm {algorithm:?}"
        ),
    ))
}

/// Invoke a no-argument virtual method returning a `String` and decode it.
fn string_from_virtual(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    name: &str,
    descriptor: &str,
) -> Option<String> {
    match ctx.invoke_virtual(obj, name, descriptor, &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// The exact class name of an object, or the empty string when it cannot be
/// resolved.
fn class_name_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let class_id = ctx.class_id_of_object(obj);
    ctx.class_name_of_id(class_id).unwrap_or_default()
}

/// Whether the SPI this call landed on can hold a `SecretKeyEntry` at all.
///
/// PKCS#12 has a SecretBag; JKS has no representation for one, and real
/// `JavaKeyStore.engineSetKeyEntry` answers exactly
/// `KeyStoreException("Cannot store non-PrivateKeys")`. The check is by
/// receiver class rather than a flag threaded through `register_engine_surface`
/// because `NativeCallback` is a bare `fn` pointer and cannot capture the FQN
/// it was registered under.
fn spi_supports_secret_keys(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    !class_name_of(ctx, this).starts_with("sun/security/provider/JavaKeyStore")
}

/// Read `KeyStore.PasswordProtection.getPassword()` off a protection parameter.
/// `Ok(None)` means "not a `PasswordProtection`", which every caller has to
/// answer differently from "a `PasswordProtection` carrying a null password".
fn protection_password(
    ctx: &mut dyn NativeContext,
    prot: ObjectRef,
) -> Result<Option<Value>, MethodCallFailed> {
    if class_name_of(ctx, prot) != "java/security/KeyStore$PasswordProtection" {
        return Ok(None);
    }
    match ctx.invoke_virtual(prot, "getPassword", "()[C", &[])? {
        Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// engineSetEntry(String, KeyStore.Entry, KeyStore.ProtectionParameter).
///
/// WHY THIS HAS TO BE A NATIVE — this is the whole
/// `pkcs12-setentry-secretkeyentry-is-a-silent-noop-20260805` defect:
/// `KeyStore.setEntry` is real bytecode and delegates straight here, but real
/// `PKCS12KeyStore.engineSetEntry` does NOT route through the public
/// `engineSetKeyEntry`/`engineSetCertificateEntry` this module already
/// intercepts. It calls its own PRIVATE `setKeyEntry`/`setCertEntry`, which
/// mutate the SPI object's own `entries` map. Every read on this VM
/// (`engineAliases`, `engineSize`, `engineContainsAlias`, `engineGetKey`,
/// `engineStore`) is served from this file's side table instead, so the real
/// mutation landed somewhere nothing ever looks: `setEntry` returned normally,
/// threw nothing, and the entry was already gone before anything was
/// serialised. All three entry kinds were affected, not only `SecretKeyEntry`.
///
/// The decision tree below is `PKCS12KeyStore.engineSetEntry`'s, message for
/// message, including the two `KeyStoreException`s that are the only correct
/// answer for a missing password — a caller that checks for an exception has
/// to be told, and telling it is the entire point.
///
/// KNOWN LIMITATION, stated rather than hidden: the per-entry password is
/// accepted and validated but not retained; entries are held in the clear in
/// the side table and re-protected with the STORE password by `engineStore`.
/// That is the same limitation `jks_protect_key` documents for JKS, and it is
/// invisible to any caller that uses one password for both (every fixture, and
/// the overwhelmingly common case).
fn engine_set_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let alias = alias_arg(ctx, args)?;
    let entry = match args.get(2) {
        Some(Value::Object(Some(e))) => *e,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("KeyStore.setEntry: entry is null".into()),
            }
            .into())
        }
    };

    // A protection parameter that is present but not a `PasswordProtection` is
    // rejected before anything is stored, exactly like the real SPI.
    let mut password: Option<Value> = None;
    if let Some(Value::Object(Some(prot))) = args.get(3) {
        match protection_password(ctx, *prot)? {
            None => {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "unsupported protection parameter",
                ))
            }
            Some(Value::Object(None)) => {}
            Some(v) => password = Some(v),
        }
    }

    // `this` IS the SPI here -- this is an `engine*` callback -- so the id is
    // read off the object the fifteen sibling callbacks read theirs off.
    let id = ensure_store_id_on(ctx, this);
    let entry_class = class_name_of(ctx, entry);
    match entry_class.as_str() {
        "java/security/KeyStore$TrustedCertificateEntry" => {
            if password.is_some() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "trusted certificate entries are not password-protected",
                ));
            }
            let cert = match ctx.invoke_virtual(
                entry,
                "getTrustedCertificate",
                "()Ljava/security/cert/Certificate;",
                &[],
            )? {
                Some(Value::Object(Some(c))) => c,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no trusted certificate"),
                    ))
                }
            };
            let der = certificate_der(ctx, cert);
            if der.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): no DER encoding available for this certificate \n                         — nothing was stored"
                    ),
                ));
            }
            keystore_set_cert_entry(id, &alias, der);
            Ok(None)
        }
        "java/security/KeyStore$PrivateKeyEntry" => {
            if password.is_none() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "non-null password required to create PrivateKeyEntry",
                ));
            }
            let key = match ctx.invoke_virtual(
                entry,
                "getPrivateKey",
                "()Ljava/security/PrivateKey;",
                &[],
            )? {
                Some(Value::Object(Some(k))) => k,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no private key"),
                    ))
                }
            };
            let mut key_der = read_encoded_byte_array(ctx, key);
            if key_der.is_empty() {
                // Same handle-resolution fallback `engine_set_key_entry` needs:
                // this module's own compact `PrivateKey` proxy carries a
                // `(store_id, alias_hash)` pair instead of an in-object DER.
                key_der = private_key_der_from_proxy(ctx, key).unwrap_or_default();
            }
            if key_der.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): the private key has no PKCS#8 encoding this VM \n                         can store (getEncoded() returned nothing) — nothing was stored"
                    ),
                ));
            }
            let mut chain: Vec<Vec<u8>> = Vec::new();
            if let Some(Value::Object(Some(arr))) = ctx.invoke_virtual(
                entry,
                "getCertificateChain",
                "()[Ljava/security/cert/Certificate;",
                &[],
            )? {
                let len = ctx.array_length(arr);
                for i in 0..len {
                    if let Value::Object(Some(cert)) = ctx.get_array_element(arr, i) {
                        let der = certificate_der(ctx, cert);
                        if !der.is_empty() {
                            chain.push(der);
                        }
                    }
                }
            }
            // Same TLS staging `engine_set_key_entry` performs: an in-memory
            // keystore built through setEntry is just as much a server identity
            // as one built through setKeyEntry.
            if !chain.is_empty() {
                crate::t27_tls::install_identity_from_der(&key_der, &chain);
            }
            keystore_set_key_entry(id, &alias, key_der, chain);
            Ok(None)
        }
        "java/security/KeyStore$SecretKeyEntry" => {
            if password.is_none() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "non-null password required to create SecretKeyEntry",
                ));
            }
            if !spi_supports_secret_keys(ctx, this) {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    "Cannot store non-PrivateKeys",
                ));
            }
            let key = match ctx.invoke_virtual(
                entry,
                "getSecretKey",
                "()Ljavax/crypto/SecretKey;",
                &[],
            )? {
                Some(Value::Object(Some(k))) => k,
                _ => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/security/KeyStoreException",
                        &format!("setEntry({alias:?}): entry carries no secret key"),
                    ))
                }
            };
            let raw = read_encoded_byte_array(ctx, key);
            if raw.is_empty() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/security/KeyStoreException",
                    &format!(
                        "setEntry({alias:?}): the secret key has no RAW encoding this VM can \n                         store (getEncoded() returned nothing) — nothing was stored"
                    ),
                ));
            }
            let algorithm = string_from_virtual(ctx, key, "getAlgorithm", "()Ljava/lang/String;")
                .unwrap_or_else(|| "AES".to_string());
            require_encodable_secret_alg(ctx, &algorithm)?;
            keystore_set_secret_key_entry(id, &alias, raw, &algorithm);
            Ok(None)
        }
        other => {
            let dotted = other.replace('/', ".");
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/KeyStoreException",
                &format!("unsupported entry type: {dotted}"),
            ))
        }
    }
}

/// engineGetEntry(String, KeyStore.ProtectionParameter).
///
/// The companion to [`engine_set_entry`], and needed for the same reason: real
/// `PKCS12KeyStore.engineGetEntry` reads the SPI object's own `entries` map,
/// which on this VM is empty because every mutation lands in this file's side
/// table. Leaving it to real bytecode meant `getEntry` answered `null` for an
/// alias that `containsAlias`/`aliases()`/`getKey` all reported as present, and
/// `UnrecoverableKeyException` for a trusted-cert entry that
/// `setCertificateEntry` had just stored successfully.
///
/// The algorithm is `KeyStoreSpi.engineGetEntry`'s, which is defined purely in
/// terms of the `engine*` accessors this module already owns.
fn engine_get_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;

    let Some(store) = keystore_lookup(id) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(entry) = store.entries.get(&alias) else {
        return Ok(Some(Value::Object(None)));
    };
    let is_cert_entry = matches!(entry.kind, EntryKind::TrustedCert { .. });
    let chain_len = match &entry.kind {
        EntryKind::PrivateKey { chain, .. } => chain.len(),
        _ => 0,
    };
    let is_secret = matches!(entry.kind, EntryKind::SecretKey { .. });

    let prot = match args.get(2) {
        Some(Value::Object(Some(p))) => Some(*p),
        _ => None,
    };

    let Some(prot) = prot else {
        if !is_cert_entry {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/UnrecoverableKeyException",
                "requested entry requires a password",
            ));
        }
        let cert = engine_get_certificate(ctx, args)?;
        let cert = match cert {
            Some(v @ Value::Object(Some(_))) => v,
            _ => return Ok(Some(Value::Object(None))),
        };
        return ctx.new_object_initialized(
            "java/security/KeyStore$TrustedCertificateEntry",
            "(Ljava/security/cert/Certificate;)V",
            &[cert],
        );
    };

    let Some(password) = protection_password(ctx, prot)? else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/UnsupportedOperationException",
            "unsupported protection parameter",
        ));
    };
    if is_cert_entry {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/UnsupportedOperationException",
            "trusted certificate entries are not password-protected",
        ));
    }

    let key_args = [
        Value::Object(Some(this)),
        args.get(1).copied().unwrap_or(Value::Object(None)),
        password,
    ];
    let key = match engine_get_key(ctx, &key_args)? {
        Some(v @ Value::Object(Some(_))) => v,
        _ => return Ok(Some(Value::Object(None))),
    };

    if is_secret {
        return ctx.new_object_initialized(
            "java/security/KeyStore$SecretKeyEntry",
            "(Ljavax/crypto/SecretKey;)V",
            &[key],
        );
    }

    if chain_len == 0 {
        // Unreachable through any API real JDK permits — `PKCS12KeyStore`
        // refuses to store a private key without a chain, and
        // `PrivateKeyEntry`'s constructor rejects a zero-length one. Say so
        // instead of letting the constructor throw an
        // `IllegalArgumentException` that names neither the alias nor the
        // keystore.
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/KeyStoreException",
            &format!(
                "getEntry({alias:?}): this private-key entry has no certificate chain, so no \n                 PrivateKeyEntry can be built for it"
            ),
        ));
    }
    let chain = match engine_get_certificate_chain(ctx, args)? {
        Some(v @ Value::Object(Some(_))) => v,
        _ => Value::Object(None),
    };
    ctx.new_object_initialized(
        "java/security/KeyStore$PrivateKeyEntry",
        "(Ljava/security/PrivateKey;[Ljava/security/cert/Certificate;)V",
        &[key, chain],
    )
}

/// engineDeleteEntry(String alias) -- in-memory removal (companion to
/// engine_set_certificate_entry; same side-table consistency rationale).
pub(crate) fn engine_delete_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let alias = alias_arg(ctx, args)?;
    keystore_delete_entry(id, &alias);
    Ok(None)
}

/// engineStore(OutputStream, char[]) — serialise the side-table to the JKS wire
/// format and write it to the stream. The read natives are backed by the CV
/// side-table; real PKCS12KeyStore.engineStore would serialise its own (empty)
/// `entries` field, so a round-trip (store → load → aliases) reported 0 entries
/// (keycloak TruststoreBuilderTest.testMergedTrustStore). Writing JKS (which
/// CV's load path detects by magic and parses via load_jks) round-trips the
/// entries through CV regardless of the declared KeyStore type.
/// Hand `bytes` to a Java `OutputStream` via `write(byte[])`.
///
/// Extracted so the three format arms of [`engine_store`] share one emitter
/// rather than three copies of the array-building loop.
fn write_bytes_to_stream(
    ctx: &mut dyn NativeContext,
    out: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    ctx.invoke_virtual(out, "write", "([B)V", &[Value::Object(Some(arr))])?;
    Ok(None)
}

pub(crate) fn engine_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = this_arg(args)?;
    let id = get_store_id(ctx, this);
    let out = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // JKS DEMANDS a store password; PKCS#12 permits a null one.
    //
    // MEASURED in both modes (`probes/KeyStoreFamilySweep.java`), and it is one
    // of the places the module's own header claim -- "PKCS12 + JKS share the
    // same `engine*` surface" -- is false:
    //
    //   ks.store(out, null)
    //     JKS      HotSpot IllegalArgumentException   CratonVM no-throw
    //     PKCS12   HotSpot no-throw                   CratonVM no-throw
    //
    // Real `JavaKeyStore.engineStore` opens with `if (password == null) throw
    // new IllegalArgumentException("password can't be null")`; `PKCS12KeyStore`
    // has no such line and treats null as "no encryption of the MAC". Keyed on
    // the receiver class for the same reason `spi_supports_secret_keys` is --
    // `NativeCallback` is a bare `fn` pointer and cannot capture the FQN it was
    // registered under.
    //
    // Note this is NOT the same axis as the format choice below: that one is
    // decided by CONTENT (a SecretKeyEntry forces PKCS#12), while the refusal
    // is decided by the SPI the caller actually asked for.
    if matches!(args.get(2), None | Some(Value::Object(None)))
        && class_name_of(ctx, this).starts_with("sun/security/provider/JavaKeyStore")
    {
        return Err(RuntimeError::IllegalArgumentException {
            message: "password can't be null".to_string(),
        }
        .into());
    }
    let password = match args.get(2) {
        Some(v) => read_password(ctx, v),
        None => Vec::new(),
    };
    let store = keystore_lookup(id).unwrap_or_default();
    // Format choice, stated plainly: JKS cannot represent a `SecretKeyEntry`
    // at all, so a store holding one has to go out as PKCS#12 or lose the
    // entry -- which is exactly what this did, silently, until now. Stores
    // WITHOUT a secret key keep the byte-for-byte JKS body, because that is
    // the shape every already-validated round trip in this VM (the keycloak
    // truststore merge, the TLS identity paths) is measured against, and CV's
    // loader detects either format by magic on the way back in.
    // FORMAT BY DECLARED TYPE FIRST. The SPI class the caller actually got is
    // right here -- the refusal above already reads it -- and it is the only
    // thing that says which format the caller ASKED for.
    //
    // This used to be decided by CONTENT alone, with the reason stated: "CV's
    // loader detects either format by magic on the way back in". That is true
    // of CV's loader, and it is the whole premise: it assumes nothing else will
    // ever read the file. So `KeyStore.getInstance("PKCS12").store(..)` emitted
    // a JKS file -- first four bytes `feedfeed` where HotSpot writes a DER
    // SEQUENCE -- and every PKCS12 keystore this VM produced was unreadable by
    // keytool, OpenSSL and every other JVM. MEASURED by
    // `apps/probes/KeyStoreTypeProbe.java`.
    //
    // `write_pkcs12` already existed and was already under test; it simply was
    // not reachable from the declared type.
    let spi = class_name_of(ctx, this);
    if spi.starts_with("sun/security/pkcs12/PKCS12KeyStore") {
        let bytes = write_pkcs12(&store, &password).map_err(|message| {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IOException { message },
            ))
        })?;
        return write_bytes_to_stream(ctx, out, &bytes);
    }
    if spi.starts_with("com/sun/crypto/provider/JceKeyStore") {
        let bytes = write_jks_with_magic(&store, &password, JCEKS_MAGIC);
        return write_bytes_to_stream(ctx, out, &bytes);
    }
    let bytes = if store
        .entries
        .values()
        .any(|e| matches!(e.kind, EntryKind::SecretKey { .. }))
    {
        match write_pkcs12(&store, &password) {
            Ok(bytes) => bytes,
            Err(message) => {
                // Never fall back to JKS here: that would drop the secret key
                // and hand the caller a valid-looking file, which is the
                // original defect.
                return Err(MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Runtime(RuntimeError::IOException { message }),
                ));
            }
        }
    } else {
        write_jks(&store, &password)
    };
    write_bytes_to_stream(ctx, out, &bytes)
}

// ---------------------------------------------------------------------------
// Helpers — store-id stash, mirror allocation, alias decode
// ---------------------------------------------------------------------------

/// Extract the alias `String` argument of an `engine*` callback.
///
/// The alias is the second arg (`args.get(1)`) of every alias-taking
/// `engine*` method (`engineGetKey`, `engineGetCertificate`, …). It arrives
/// as a `Value::Object(Some(string_ref))`; we decode it through the
/// `NativeContext::read_string` accessor the rest of `native-builtins` uses
/// (e.g. `log4j_extras::extract_logger_name`). Returns `None` for a null /
/// non-string arg so the caller can `.unwrap_or_default()` to the empty
/// alias, matching real-JDK's behaviour of treating a null alias as "no such
/// entry".
fn read_string_arg(ctx: &mut dyn NativeContext, v: &Value) -> Option<String> {
    match v {
        Value::Object(Some(o)) => ctx.read_string(*o),
        _ => None,
    }
}

/// Copy a Java `byte[]` out of the heap. Returns an empty vec for anything
/// that is not a non-empty array.
fn read_byte_array(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        }
    }
    out
}

/// Invoke `getEncoded()[B` on `obj` and copy the result out. Empty vec when the
/// call fails, returns null, or returns a zero-length array.
pub(crate) fn read_encoded_byte_array(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Vec<u8> {
    match ctx.invoke_virtual(obj, "getEncoded", "()[B", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => read_byte_array(ctx, arr),
        _ => Vec::new(),
    }
}

/// DER encoding of a `java.security.cert.Certificate`.
///
/// Prefers the real `Certificate.getEncoded()`. Falls back to the raw DER this
/// module itself stashes in slot 3 of the synthetic
/// `java/security/cert/X509Certificate` mirror ([`make_x509_mirror`]): in
/// synthetic-JDK mode the registered `X509Certificate.getEncoded()` shim
/// (`phases_late::ssl_security`) hands back an EMPTY array unless the
/// `legacy-synthetic-crypto` cert store is compiled in, so the virtual call
/// alone loses the very bytes we put there. Without the fallback,
/// `setCertificateEntry` on a mirror-backed certificate stored nothing and
/// `aliases()`/`size()` reported an empty keystore.
///
/// The fallback is deliberately narrow — it fires only for objects whose exact
/// class is the synthetic mirror — so it can never misread an unrelated field
/// of a real `sun.security.x509.X509CertImpl`.
pub(crate) fn certificate_der(ctx: &mut dyn NativeContext, cert: ObjectRef) -> Vec<u8> {
    let der = read_encoded_byte_array(ctx, cert);
    if !der.is_empty() {
        return der;
    }
    if !matches!(ctx.heap_kind_of(cert), cratonvm_types::ObjectKind::Object) {
        return Vec::new();
    }
    if ctx.object_num_fields(cert) <= 3 {
        return Vec::new();
    }
    let cls_id = ctx.class_id_of_object(cert);
    match ctx.class_name_of_id(cls_id) {
        Some(name) if name == "java/security/cert/X509Certificate" => {
            match ctx.get_field(cert, 3) {
                Value::Object(Some(arr)) => read_byte_array(ctx, arr),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Get this keystore's `KEYSTORE_REGISTRY` id, allocating and stamping an empty
/// store when it has none yet.
///
/// `get_store_id` answers 0 for a keystore that was never `engineLoad`ed, and
/// every mutator (`keystore_set_cert_entry`, …) silently no-ops on id 0. The
/// synthetic-JDK `java.security.KeyStore` in `tls.rs` needs a store the moment
/// its first entry is set, so give it one on demand; `set_store_id` records the
/// id on the object's slot/named field AND in the identity side-table, so every
/// later read resolves the same store.
pub(crate) fn keystore_ensure_store_id(ctx: &mut dyn NativeContext, ks_obj: ObjectRef) -> i32 {
    let target = unwrap_keystore_spi(ctx, ks_obj);
    ensure_store_id_on(ctx, target)
}

/// [`keystore_ensure_store_id`] without the unwrap, for a caller that already
/// holds the SPI.
///
/// Every other `engine*` callback in this file reads its id with a plain
/// `get_store_id(ctx, this)`; `engine_set_entry` was the one that went through
/// the unwrapping form, and on a `KeyStoreDelegator` receiver that is how it
/// came to write its entries somewhere no reader looks. Asking for the id of
/// the object you were handed is the invariant the other fifteen callbacks
/// hold, so it is spelled out here rather than left to the unwrap being
/// harmless.
fn ensure_store_id_on(ctx: &mut dyn NativeContext, target: ObjectRef) -> i32 {
    let existing = get_store_id(ctx, target);
    if existing != 0 {
        return existing;
    }
    let id = keystore_register(LoadedKeyStore::default());
    set_store_id(ctx, target, id);
    id
}

/// True when `id`'s store currently holds `alias`. Used by the public
/// `KeyStore` shim to verify a mutation actually landed before reporting
/// success (a dropped entry must surface as `KeyStoreException`, not silence).
pub(crate) fn keystore_has_alias(id: i32, alias: &str) -> bool {
    registry()
        .read()
        .stores
        .get(&id)
        .map(|s| s.entries.contains_key(alias))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Public `java.security.KeyStore` shims
//
// The public `KeyStore` wrapper is intercepted by `tls.rs` (synthetic-JDK mode)
// and `phases_early.rs`. Both must reach THIS module's registry-backed engine
// surface rather than reimplementing storage, otherwise the public API and the
// SPI disagree about what the keystore contains. Each shim unwraps a real
// `keyStoreSpi` delegate when there is one (so we drive the same object
// `engineLoad` stamped) and otherwise operates on the wrapper itself — the
// synthetic `KeyStore` has no SPI and IS its own store. Same shape as
// `keystore_aliases`/`keystore_size` above.
// ---------------------------------------------------------------------------

/// Rewrite `args[0]` to the SPI delegate (or the receiver itself) and hand the
/// call to one of the `engine_*` implementations.
fn via_spi(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    engine: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) -> MethodCallResult {
    let this = this_arg(args)?;
    let spi = unwrap_keystore_spi(ctx, this);
    let mut engine_args = args.to_vec();
    engine_args[0] = Value::Object(Some(spi));
    engine(ctx, &engine_args)
}

/// `KeyStore.load(InputStream, char[])` → `engineLoad`.
pub(crate) fn keystore_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_load)
}

/// `KeyStore.getKey(String, char[])` → `engineGetKey`.
pub(crate) fn keystore_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_get_key)
}

/// `KeyStore.getCertificate(String)` → `engineGetCertificate`.
pub(crate) fn keystore_get_certificate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_get_certificate)
}

/// `KeyStore.getCertificateChain(String)` → `engineGetCertificateChain`.
pub(crate) fn keystore_get_certificate_chain(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_get_certificate_chain)
}

/// `KeyStore.containsAlias(String)` → `engineContainsAlias`.
pub(crate) fn keystore_contains_alias(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_contains_alias)
}

/// `KeyStore.isKeyEntry(String)` → `engineIsKeyEntry`.
pub(crate) fn keystore_is_key_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_is_key_entry)
}

/// `KeyStore.isCertificateEntry(String)` → `engineIsCertificateEntry`.
pub(crate) fn keystore_is_certificate_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_is_certificate_entry)
}

/// `KeyStore.setCertificateEntry(String, Certificate)` → the SPI mutator.
/// (`_native` suffix: the plain names belong to this module's registry-level
/// `keystore_set_cert_entry` / `keystore_set_key_entry` helpers.)
pub(crate) fn keystore_set_certificate_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_set_certificate_entry)
}

/// `KeyStore.setKeyEntry(String, Key, char[], Certificate[])` → the SPI mutator.
pub(crate) fn keystore_set_key_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_set_key_entry)
}

/// `KeyStore.deleteEntry(String)` → `engineDeleteEntry`.
pub(crate) fn keystore_delete_entry_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    via_spi(ctx, args, engine_delete_entry)
}

/// `KeyStore.store(OutputStream, char[])` → `engineStore` (the JKS writer).
pub(crate) fn keystore_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    via_spi(ctx, args, engine_store)
}

pub(crate) fn make_x509_mirror(
    ctx: &mut dyn NativeContext,
    alias: &str,
    cert_der: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    // Build the DER byte[] once — used either as the ctor arg for the real cert
    // or stashed in the synthetic-mirror fallback.
    let arr = ctx.new_array(ArrayElementType::Byte, cert_der.len());
    for (i, b) in cert_der.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    // Prefer a REAL `sun.security.x509.X509CertImpl` parsed from the DER. A bare
    // synthetic `java/security/cert/X509Certificate` is the ABSTRACT class, so
    // the SunX509 KeyManager's chain validation — `cert.checkValidity(Date)`
    // during init — throws `AbstractMethodError` ("no Code attribute"). The real
    // X509CertImpl implements the full X509Certificate API (checkValidity,
    // getSubjectX500Principal, getPublicKey, getEncoded, …) via real bytecode.
    if let Ok(Some(Value::Object(Some(real)))) = ctx.new_object_initialized(
        "sun/security/x509/X509CertImpl",
        "([B)V",
        &[Value::Object(Some(arr))],
    ) {
        return Ok(real);
    }
    // Fallback: synthetic mirror (subject/issuer = alias, DER in slot 3). Reached
    // only if the real DER parse fails (e.g. an unimplemented DerValue native).
    let cert_obj = try_alloc_concurrent_synthetic(ctx, "java/security/cert/X509Certificate", 4)?;
    // `arr` and `cert_obj` are both live across `create_string` below, which
    // allocates and can therefore relocate either of them under a moving young
    // collection — pin both and re-read through the pins. Same Family-1 shape
    // as the chain-array fill in `t27_tls::engine_run_trust_check`; a store
    // through a stale ref is silently DROPPED by the heap guard, which here
    // would leave the mirror with a null DER and no subject/issuer.
    let arr_pin = ctx.pin_native_root(arr);
    let cert_pin = ctx.pin_native_root(cert_obj);
    // Field layout matches what `phases_early.rs` uses for the
    // `getCertificate` path: 0=subject string, 1=issuer string, 2=cert_id,
    // 3=DER (byte[]). Subject + issuer here are alias strings — the real
    // X.509 CN extraction lives in `security_manager/x509.rs`, which TLS
    // can call once it has the DER. We keep the alias as a stand-in so
    // tests asserting on `getName()` see something stable.
    let alias_str = ctx.create_string(alias);
    let cert_obj = ctx.read_native_pin(cert_pin, cert_obj);
    ctx.set_field(cert_obj, 0, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 1, Value::Object(Some(alias_str)));
    ctx.set_field(cert_obj, 2, Value::Int(0));

    // Stash the DER (reuse the byte[] built above) so consumers can call
    // `Certificate.getEncoded()` or pass the bytes to a TLS/`X509TrustManager`.
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(cert_obj, 3, Value::Object(Some(arr)));
    ctx.unpin_native_roots(arr_pin);
    Ok(cert_obj)
}

/// Identity-keyed fallback for the store-id, used when the KeyStoreSpi object
/// has no field to hold it. A real `sun.security.provider.JavaKeyStore$JKS` has
/// a SINGLE instance field (`entries`), so neither the synthetic
/// `cratonvm$keystore$storeId` field (which doesn't exist on the real class →
/// `set_field_by_name` no-ops) nor slot `FIELD_STORE_ID=4` (out of range) can
/// store it — `engineAliases` then looked up store 0 and reported an empty
/// keystore, so KeyManagerFactory found "No aliases for private keys" and TLS
/// init failed. `identity_hash_code` is stable across GC, so key on it.
///
/// The key is `(NativeContext::vm_identity(), identity_hash_code(spi))`, not a
/// bare identity hash: a hash code is unique only *within one heap* while this
/// table is a process-global `static`. Rust tests (and any embedder) create
/// several independent `Vm`s in one process, so a bare-`i32` key let VM B's
/// KeyStoreSpi resolve to VM A's store id and read another VM's keys and
/// certificates out of `registry()`. Same rule as
/// `crypto_impl::RSA_REALKEY_MAP` / `jca::signature::SigKey`
/// (`native-api/src/registry.rs`, `vm_identity` doc). Values are plain `i32`
/// store ids -- no `ObjectRef`s, so no collector scan/remap companion is
/// needed. (`store_identity_pem_map` below needs no VM component: its key is a
/// `registry()` store id drawn from a single process-wide counter, so it is
/// already globally unique.)
#[allow(clippy::type_complexity)]
fn store_id_by_identity() -> &'static std::sync::Mutex<std::collections::HashMap<(usize, i32), i32>>
{
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<(usize, i32), i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `store_id` → (cert_pem, key_pem) for the keystore's first key entry. Recorded
/// at `engineLoad`; consumed by `keystore_set_pending_km_identity` (called from
/// `KeyManagerFactory.init`) to drive the per-`SSLContext` mTLS identity flow in
/// `t27_tls`.
#[allow(clippy::type_complexity)]
fn store_identity_pem_map(
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, (String, String)>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<i32, (String, String)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Unwrap a `java.security.KeyStore` wrapper down to its real `KeyStoreSpi`
/// instance (the private `keyStoreSpi` field, real KeyStore's 3rd of 3 fields:
/// `type`, `provider`, `keyStoreSpi`). `KeyManagerFactory.init(KeyStore,
/// char[])` (and `TrustManagerFactory.init(KeyStore)`) receive the outer
/// wrapper, but `engineLoad`/`engineSetKeyEntry`/etc. run natively on the SPI
/// object itself (registered on the SPI's own class name) and stamp the
/// store-id side-table keyed off the SPI's identity -- calling
/// `get_store_id` directly on the wrapper looks at the WRONG object (the
/// wrapper's own fields/identity hash have nothing to do with the SPI's), so
/// it always read back id=0 and every staged identity was silently dropped.
/// Falls back to the wrapper object itself if the field can't be read (e.g.
/// a non-standard `KeyStore` subclass), preserving old behaviour for any
/// caller that doesn't hit this real-bytecode shape.
fn unwrap_keystore_spi(ctx: &mut dyn NativeContext, keystore_obj: ObjectRef) -> ObjectRef {
    if let Value::Object(Some(spi)) = ctx.get_field_by_name(keystore_obj, "keyStoreSpi") {
        return spi;
    }
    // Fallback: real KeyStore's field order is type(0)/provider(1)/keyStoreSpi(2).
    //
    // THE INDEX IS BLIND, SO WHAT IT PRODUCES IS CHECKED BEFORE IT IS BELIEVED.
    // This helper is also reached with a receiver that is ALREADY an SPI --
    // `keystore_ensure_store_id` is called from `engine_set_entry`, whose
    // `this` is the provider's own `KeyStoreSpi`. `KeyStore.getInstance("PKCS12")`
    // resolves to `sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12`, a
    // `sun/security/util/KeyStoreDelegator` whose slot 2 is `primaryKeyStore`:
    // a `java.lang.Class` MIRROR, not a keystore. The name lookup above misses
    // (a delegator has no `keyStoreSpi` field), the index fired, and every
    // `setEntry` was stored against the store id of a process-global class
    // mirror -- which no reader ever asks. `setEntry` returned normally,
    // `size()`/`aliases()`/`getKey()` answered as if nothing had been stored,
    // and `store()` serialised an empty keystore. Measured on JDK 25, both
    // modes, `apps/probes/JdkOnlyPlatformProbe.java`'s `security` section.
    //
    // So the candidate must actually BE a `KeyStoreSpi`. When that class
    // cannot be resolved at all the historical blind behaviour is kept rather
    // than silently changed on an image that has no such class.
    if ctx.object_num_fields(keystore_obj) > 2 {
        if let Value::Object(Some(spi)) = ctx.get_field(keystore_obj, 2) {
            let Some(spi_cid) = ctx.class_id_by_name("java/security/KeyStoreSpi") else {
                return spi;
            };
            let actual = ctx.class_id_of_object(spi);
            if actual == spi_cid || ctx.is_subclass(actual, spi_cid) {
                return spi;
            }
        }
    }
    keystore_obj
}

/// `KeyManagerFactory.init(KeyStore, char[])` calls this so the keystore's
/// identity is staged for the next `SSLContext.init` on this thread (see
/// `t27_tls::set_pending_km_identity`).
pub(crate) fn keystore_set_pending_km_identity(
    ctx: &mut dyn NativeContext,
    keystore_obj: ObjectRef,
) {
    keystore_set_pending_km_identity_with_password(ctx, keystore_obj, &[]);
}

/// Same as [`keystore_set_pending_km_identity`], with the KeyManagerFactory's
/// per-entry password. JKS permits a key password distinct from the store-load
/// password; Spring/Tomcat use that arrangement for their `test.jks` fixture.
pub(crate) fn keystore_set_pending_km_identity_with_password(
    ctx: &mut dyn NativeContext,
    keystore_obj: ObjectRef,
    password: &[u8],
) {
    let spi = unwrap_keystore_spi(ctx, keystore_obj);
    let id = get_store_id(ctx, spi);
    if id == 0 {
        return;
    }
    keystore_unlock_private_keys(id, password);
    if password.is_empty() {
        let ident = store_identity_pem_map().lock().unwrap().get(&id).cloned();
        if let Some((cert, key)) = ident {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] keystore_set_pending_km_identity_with_password store_id={} SOURCE=cached-snapshot cert_pem_len={} key_pem_len={}",
                    id, cert.len(), key.len()
                );
            }
            crate::t27_tls::set_pending_km_identity(cert, key);
            return;
        }
    }
    // FIX (httpserver-pkcs12-20260706): store_identity_pem_map is a
    // load-time snapshot (populated only by engineLoad's entries scan). A
    // keystore built via getInstance()+load(null,null)+setKeyEntry(...) --
    // Netty's JdkSslServerContext.buildKeyStore ephemeral self-signed-cert
    // pattern -- has an empty snapshot at that id (load(null,null) sees zero
    // entries) even though setKeyEntry has since added a real PrivateKey
    // entry via keystore_set_key_entry. Fall back to scanning the LIVE
    // registry (keystore_get_first_private_key) so this identity isn't
    // silently dropped -- without this, SSLContext.init produced a
    // server-side context with no certificate to present and the TLS
    // handshake failed immediately.
    if let Some((key_der, chain)) = keystore_get_first_private_key(id) {
        let (cert_pem, key_pem) = crate::t27_tls::der_identity_to_pem(&key_der, &chain);
        store_identity_pem_map()
            .lock()
            .unwrap()
            .insert(id, (cert_pem.clone(), key_pem.clone()));
        crate::t27_tls::set_pending_km_identity(cert_pem, key_pem);
    }
}

/// Materialize JKS private-key entries when their per-entry password becomes
/// available. `jks_recover_key` authenticates the plaintext, so it is safe to
/// attempt this on PKCS#8/P12 entries too: non-JKS data simply remains intact.
fn keystore_unlock_private_keys(id: i32, password: &[u8]) {
    if password.is_empty() {
        return;
    }
    let mut stores = registry().write();
    let Some(store) = stores.stores.get_mut(&id) else {
        return;
    };
    let mut installed_identity = false;
    for entry in store.entries.values_mut() {
        if let EntryKind::PrivateKey { key_der, chain } = &mut entry.kind {
            if let Some(plain) = jks_recover_key(key_der, password) {
                *key_der = plain;
                if !installed_identity {
                    if crate::nbflags().dbg_tls_hs {
                        eprintln!(
                            "[dbg-tls-hs] install_identity_from_der CALLER=keystore_unlock_private_keys(store_id={}) key_len={}",
                            id,
                            key_der.len()
                        );
                    }
                    crate::t27_tls::install_identity_from_der(key_der, chain);
                    installed_identity = true;
                }
            }
        }
    }
}
/// Fetch the PKCS#8 DER + cert chain of the first `PrivateKey` entry in a
/// registered store, scanning the LIVE registry rather than the load-time
/// snapshot `store_identity_pem_map` relies on. See
/// `keystore_set_pending_km_identity`'s fallback for why this is needed.
fn keystore_get_first_private_key(id: i32) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let store = registry().read().stores.get(&id).cloned()?;
    if crate::nbflags().dbg_tls_hs {
        let summary: Vec<String> = store
            .entries
            .iter()
            .map(|(alias, e)| match &e.kind {
                EntryKind::PrivateKey { key_der, chain } => format!(
                    "{alias}=PrivateKey(key_len={},chain_cert_lens={:?})",
                    key_der.len(),
                    chain.iter().map(|c| c.len()).collect::<Vec<_>>()
                ),
                EntryKind::TrustedCert { cert_der } => {
                    format!("{alias}=TrustedCert(len={})", cert_der.len())
                }
                EntryKind::SecretKey { .. } => format!("{alias}=SecretKey"),
            })
            .collect();
        eprintln!(
            "[dbg-tls-hs] keystore_get_first_private_key store_id={} entries={:?}",
            id, summary
        );
    }
    for (alias, entry) in store.entries.iter() {
        if let EntryKind::PrivateKey { key_der, chain } = &entry.kind {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] keystore_get_first_private_key store_id={} PICKED alias={:?}",
                    id, alias
                );
            }
            return Some((key_der.clone(), chain.clone()));
        }
    }
    None
}

fn get_store_id(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    // Try multiple storage strategies; the synthetic class allocation pattern
    // means our `engine*` callbacks may run on either a real-JDK PKCS12KeyStore
    // mirror (with all the real fields) or a 5-field synthetic mirror that
    // `tls.rs` allocates. Probe by name first, fall back to the conventional
    // slot index, then the identity side-table.
    let by_name = ctx.get_field_by_name(this, "cratonvm$keystore$storeId");
    if let Value::Int(i) = by_name {
        if i != 0 {
            return i;
        }
    }
    if let Value::Long(l) = by_name {
        if l != 0 {
            return l as i32;
        }
    }
    let n = ctx.object_num_fields(this);
    if n > FIELD_STORE_ID {
        match ctx.get_field(this, FIELD_STORE_ID) {
            Value::Int(i) if i != 0 => return i,
            Value::Long(l) if l != 0 => return l as i32,
            _ => {}
        }
    }
    // Identity side-table fallback (real JKS objects have no field for it).
    // VM-scoped key -- see `store_id_by_identity`'s doc comment.
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        let k = (ctx.vm_identity(), ih);
        if let Some(&id) = store_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&k)
        {
            return id;
        }
    }
    0
}

/// FIX (tls-residuals): the robust, three-tier id lookup `tls.rs`'s own doc
/// comment on `read_keystore_registry_id` asked for ("a `pub fn
/// keystore::keystore_id_from_object` ... that reuses `get_store_id`").
/// `x509_manager::read_keystore_id`/`tls::read_keystore_registry_id` each
/// re-implemented a SUBSET of `get_store_id`'s lookup (named field + a slot
/// index) but never the identity-hash side-table tier — the ONLY tier that
/// resolves a real `java.security.KeyStore` object in real-JDK mode, since
/// its real 4-field layout (`type`/`provider`/`keyStoreSpi`/`initialized`)
/// has no room for a pseudo-field and `set_field_by_name` no-ops on a real
/// class. Without this, `TrustManagerFactory.init(KeyStore)` (both the
/// SimpleFactory and PKIXFactory SPI paths) always resolved id 0 for a
/// caller-supplied truststore, silently falling back to the ~100+ platform
/// root certs instead of the caller's actual (possibly test-only/private) CA
/// — the client then rejects ANY peer certificate signed by that CA with
/// `UnknownIssuer`, regardless of any other TLS/OCSP configuration.
pub(crate) fn keystore_id_from_object(ctx: &mut dyn NativeContext, ks_obj: ObjectRef) -> i32 {
    let id = get_store_id(ctx, ks_obj);
    if id != 0 {
        return id;
    }
    // `engineLoad` (this file) is registered on the KeyStoreSpi delegate
    // class, so `set_store_id`'s identity-hash tier stamps the SPI
    // instance's identity — a DIFFERENT object from the public
    // `java.security.KeyStore` wrapper callers like
    // `TrustManagerFactory.init(KeyStore)` actually receive. Re-run the same
    // lookup against the wrapper's `keyStoreSpi` delegate field so that tier
    // has a chance to match.
    if let Value::Object(Some(spi)) = ctx.get_field_by_name(ks_obj, "keyStoreSpi") {
        return get_store_id(ctx, spi);
    }
    0
}

fn set_store_id(ctx: &mut dyn NativeContext, this: ObjectRef, id: i32) {
    ctx.set_field_by_name(this, "cratonvm$keystore$storeId", Value::Int(id));
    let n = ctx.object_num_fields(this);
    if n > FIELD_STORE_ID {
        ctx.set_field(this, FIELD_STORE_ID, Value::Int(id));
    }
    // Always record in the identity side-table so retrieval works even when the
    // KeyStoreSpi object has no usable field (real JavaKeyStore$JKS = 1 field).
    let ih = ctx.identity_hash_code(this);
    if ih != 0 {
        // VM-scoped key -- see `store_id_by_identity`'s doc comment.
        let k = (ctx.vm_identity(), ih);
        store_id_by_identity()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, id);
    }
}

/// Map the first byte of a PKCS#8 DER to a coarse algorithm index. RSA and
/// EC are the only two we expect from a TLS keystore. Default to RSA-ish.
fn detect_algo_idx(key_der: &[u8]) -> i32 {
    // PKCS#8 PrivateKeyInfo: SEQUENCE { Integer 0, AlgorithmIdentifier, OCTET STRING }
    // AlgorithmIdentifier: SEQUENCE { OID, optional params }. RSA OID is
    // 1.2.840.113549.1.1.1 (DER `06 09 2A 86 48 86 F7 0D 01 01 01`).
    // EC OID is `1.2.840.10045.2.1` (`06 07 2A 86 48 CE 3D 02 01`).
    let needle_rsa = [
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01,
    ];
    let needle_ec = [0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    if find_subseq(key_der, &needle_rsa).is_some() {
        return 6; // RSA
    }
    if find_subseq(key_der, &needle_ec).is_some() {
        return 7; // EC
    }
    6 // default RSA
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Hermetic tests with inlined fixture bytes. The fixtures are tiny
    //! synthetic keystores produced specifically for this test (a single
    //! self-signed leaf + one trusted-cert entry, both ≤ 4 KiB).
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    use super::*;

    // Synthesise a JKS file in-memory by writing the format directly. This
    // keeps the test free of external tooling (no need for `keytool` to be on
    // the test machine). The cert + key payloads here are deliberately tiny
    // dummy DER blobs — the JKS parser only treats them as opaque bytes.
    fn synth_jks(password: &[u8]) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&JKS_MAGIC.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes()); // version 2
        body.extend_from_slice(&2u32.to_be_bytes()); // entry_count = 2

        // Entry 1: TrustedCertEntry, alias "trusted-ca"
        body.extend_from_slice(&2u32.to_be_bytes()); // tag = 2
        let alias1 = b"trusted-ca";
        body.extend_from_slice(&(alias1.len() as u16).to_be_bytes());
        body.extend_from_slice(alias1);
        body.extend_from_slice(&123_456_789u64.to_be_bytes()); // creation date
        let cert_type = b"X.509";
        body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
        body.extend_from_slice(cert_type);
        let cert_der = b"\x30\x06DUMMY1"; // 8-byte placeholder
        body.extend_from_slice(&(cert_der.len() as u32).to_be_bytes());
        body.extend_from_slice(cert_der);

        // Entry 2: PrivateKeyEntry, alias "leaf-key"
        body.extend_from_slice(&1u32.to_be_bytes()); // tag = 1
        let alias2 = b"leaf-key";
        body.extend_from_slice(&(alias2.len() as u16).to_be_bytes());
        body.extend_from_slice(alias2);
        body.extend_from_slice(&987_654_321u64.to_be_bytes());
        let enc_key = b"\x30\x07PKCS8KEY";
        body.extend_from_slice(&(enc_key.len() as u32).to_be_bytes());
        body.extend_from_slice(enc_key);
        body.extend_from_slice(&1u32.to_be_bytes()); // chain_count
        body.extend_from_slice(&(cert_type.len() as u16).to_be_bytes());
        body.extend_from_slice(cert_type);
        let leaf_der = b"\x30\x06DUMMY2";
        body.extend_from_slice(&(leaf_der.len() as u32).to_be_bytes());
        body.extend_from_slice(leaf_der);

        // Append integrity tag.
        let mac = jks_password_mac(password, &body);
        body.extend_from_slice(&mac);
        body
    }

    // -- BER -> DER length normalisation -----------------------------------

    #[test]
    fn der_input_is_returned_unchanged() {
        // SEQUENCE { INTEGER 5 }, already definite-length.
        let der = [0x30u8, 0x03, 0x02, 0x01, 0x05];
        assert_eq!(ber_to_definite_length(&der).expect("rewrite"), der.to_vec());
    }

    #[test]
    fn indefinite_length_becomes_definite() {
        // SEQUENCE (indefinite) { INTEGER 5 } EOC
        let ber = [0x30u8, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00];
        assert_eq!(
            ber_to_definite_length(&ber).expect("rewrite"),
            vec![0x30, 0x03, 0x02, 0x01, 0x05]
        );
    }

    #[test]
    fn nested_indefinite_lengths_are_rewritten_innermost_first() {
        // SEQ(indef){ SEQ(indef){ INTEGER 5 } } - the outer length can only be
        // known once the inner one has been rewritten.
        let ber = [
            0x30u8, 0x80, 0x30, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(
            ber_to_definite_length(&ber).expect("rewrite"),
            vec![0x30, 0x05, 0x30, 0x03, 0x02, 0x01, 0x05]
        );
    }

    #[test]
    fn segmented_octet_string_is_joined_and_made_primitive() {
        // constructed OCTET STRING (indef) { OCTET STRING AA BB, OCTET STRING CC }
        let ber = [
            0x24u8, 0x80, 0x04, 0x02, 0xAA, 0xBB, 0x04, 0x01, 0xCC, 0x00, 0x00,
        ];
        assert_eq!(
            ber_to_definite_length(&ber).expect("rewrite"),
            vec![0x04, 0x03, 0xAA, 0xBB, 0xCC]
        );
    }

    #[test]
    fn long_form_lengths_survive_the_round_trip() {
        // A 200-byte OCTET STRING needs the long form (0x81 0xC8) both ways.
        let mut der = vec![0x04u8, 0x81, 0xC8];
        der.extend(std::iter::repeat(0x41).take(200));
        assert_eq!(ber_to_definite_length(&der).expect("rewrite"), der);
    }

    #[test]
    fn truncated_input_is_an_error_not_a_silent_short_read() {
        // SEQUENCE claiming 3 content bytes but carrying one.
        assert!(ber_to_definite_length(&[0x30u8, 0x03, 0x02]).is_err());
        // Indefinite length with no end-of-contents.
        assert!(ber_to_definite_length(&[0x30u8, 0x80, 0x02, 0x01, 0x05]).is_err());
    }

    #[test]
    fn detects_jks_magic() {
        let bytes = synth_jks(b"changeit");
        assert_eq!(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            JKS_MAGIC
        );
    }

    #[test]
    fn jks_roundtrip() {
        let bytes = synth_jks(b"changeit");
        let store = load_jks(&bytes, b"changeit").expect("load_jks");
        assert_eq!(store.entries.len(), 2);
        assert!(store.entries.contains_key("trusted-ca"));
        assert!(store.entries.contains_key("leaf-key"));

        match &store.entries["trusted-ca"].kind {
            EntryKind::TrustedCert { cert_der } => {
                assert_eq!(cert_der, b"\x30\x06DUMMY1");
            }
            _ => panic!("expected TrustedCert"),
        }
        match &store.entries["leaf-key"].kind {
            EntryKind::PrivateKey { key_der, chain } => {
                assert_eq!(key_der, b"\x30\x07PKCS8KEY");
                assert_eq!(chain.len(), 1);
                assert_eq!(chain[0], b"\x30\x06DUMMY2");
            }
            _ => panic!("expected PrivateKey"),
        }
    }

    #[test]
    fn jks_hmac_mismatch_rejected() {
        let mut bytes = synth_jks(b"changeit");
        // Flip a byte in the body (the entry_count high byte).
        bytes[8] ^= 0x01;
        let err = load_jks(&bytes, b"changeit").unwrap_err();
        assert!(
            matches!(
                err,
                KeyStoreError::JksMacMismatch | KeyStoreError::BadJksTag(_)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn jks_wrong_password_rejected() {
        let bytes = synth_jks(b"changeit");
        let err = load_jks(&bytes, b"wrong").unwrap_err();
        assert!(matches!(err, KeyStoreError::JksMacMismatch));
    }

    #[test]
    fn jks_empty_password_works() {
        let bytes = synth_jks(b"");
        let store = load_jks(&bytes, b"").expect("load_jks empty pw");
        assert_eq!(store.entries.len(), 2);
    }

    #[test]
    fn dispatch_picks_jks() {
        let bytes = synth_jks(b"x");
        let store = load_keystore(&bytes, b"x").expect("dispatch");
        assert!(store.entries.contains_key("trusted-ca"));
    }

    #[test]
    fn dispatch_rejects_unknown_format() {
        let err = load_keystore(b"NOTAKEYSTORE", b"").unwrap_err();
        assert!(matches!(err, KeyStoreError::UnknownFormat));
    }

    #[test]
    fn registry_round_trip() {
        let store = LoadedKeyStore {
            entries: [(
                "alpha".to_string(),
                KeyStoreEntry {
                    alias: "alpha".to_string(),
                    creation_time_ms: 42,
                    kind: EntryKind::TrustedCert {
                        cert_der: b"hi".to_vec(),
                    },
                },
            )]
            .into_iter()
            .collect(),
        };
        let id = keystore_register(store);
        assert!(id > 0);
        let got = keystore_lookup(id).expect("registered store");
        assert_eq!(got.entries.len(), 1);
        let cert = keystore_get_cert_der(id, "alpha").unwrap();
        assert_eq!(cert, b"hi");
    }

    #[test]
    fn jks_truncated_returns_error() {
        let bytes = synth_jks(b"x");
        let truncated = &bytes[..bytes.len() - 5];
        let err = load_jks(truncated, b"x").unwrap_err();
        // After moving HMAC to the front of `load_jks`, truncating the
        // trailing 5 bytes is detected as JksMacMismatch (the trailing
        // bytes interpreted as the 20-byte tag now cover real entry
        // bytes, so the SHA-1 over the now-shorter body diverges). The
        // older Truncated / BadJksTag paths are still reachable for
        // truncations large enough that the body cannot even fit the
        // 4+4+4+20 prelude.
        assert!(matches!(
            err,
            KeyStoreError::Truncated(_)
                | KeyStoreError::BadJksTag(_)
                | KeyStoreError::JksMacMismatch
        ));
    }

    #[test]
    fn pkcs12_unknown_format_rejected() {
        // Just a SEQUENCE prefix with garbage payload — should fail parse.
        let bogus = b"\x30\x05\x00\x00\x00\x00\x00";
        let err = load_pkcs12(bogus, b"x").unwrap_err();
        assert!(matches!(
            err,
            KeyStoreError::Pkcs12Parse(_) | KeyStoreError::Pkcs12MacFailed
        ));
    }

    fn synth_sha256_mac_pkcs12(password: &str) -> Vec<u8> {
        let mut pfx = p12::PFX::new(b"\x30\x00", b"\x30\x00", None, password, "test")
            .expect("test PFX generation");
        let password_bmp = pkcs12_bmp_string(password);
        let auth_safe = pfx
            .auth_safe
            .data(&password_bmp)
            .expect("unencrypted AuthSafe");
        let mac_data = pfx.mac_data.as_mut().expect("MAC data");
        let digest = Pkcs12MacDigest::Sha256;
        mac_data.mac.digest_algorithm =
            p12::AlgorithmIdentifier::OtherAlg(p12::OtherAlgorithmIdentifier {
                algorithm_type: yasna::models::ObjectIdentifier::from_slice(&[
                    2, 16, 840, 1, 101, 3, 4, 2, 1,
                ]),
                params: Some(vec![0x05, 0x00]),
            });
        let key = pkcs12_mac_kdf(
            digest,
            &password_bmp,
            &mac_data.salt,
            mac_data.iterations,
            3,
            digest.output_len(),
        );
        mac_data.mac.digest = pkcs12_hmac(digest, &key, &auth_safe);
        pfx.to_der()
    }

    #[test]
    fn pkcs12_sha256_mac_accepts_correct_password_and_rejects_wrong_one() {
        // Since JDK 8u191, SunPKCS12 defaults to HmacPBESHA256.  `p12` 0.6
        // parses that MAC but its verifier unconditionally derives a SHA-1
        // key, producing a false "wrong password" result for Spring's .p12
        // fixtures.  Exercise the exact newer-MAC shape independently of any
        // workspace-local application fixture.
        let bytes = synth_sha256_mac_pkcs12("secret");
        let pfx = p12::PFX::parse(&bytes).expect("reparse generated PFX");
        assert!(verify_pkcs12_mac(&pfx, "secret"));
        load_pkcs12(&bytes, b"secret").expect("correct SHA-256 MAC password");
        let err = load_pkcs12(&bytes, b"wrong").unwrap_err();
        assert!(matches!(err, KeyStoreError::Pkcs12MacFailed));
    }

    /// SunPKCS12 (`keytool`) leaves the top-level AuthenticatedSafe content
    /// unencrypted (`ContentInfo::Data`) and relies solely on the outer MAC
    /// for integrity — only individual `PrivateKeyEntry` bags get their own
    /// PBES2 encryption under a possibly-different entry password. `p12`
    /// crate's own `PFX::new` doesn't model this (it PBE-encrypts the whole
    /// cert `SafeContents`), so build the realistic shape by hand.
    fn synth_unencrypted_content_pkcs12(password: &str) -> Vec<u8> {
        let cert_bag = p12::SafeBag {
            bag: p12::SafeBagKind::CertBag(p12::CertBag::X509(vec![0x30, 0x03, 0x02, 0x01, 0x00])),
            attributes: vec![],
        };
        let safe_contents =
            yasna::construct_der(|w| w.write_sequence_of(|w| cert_bag.write(w.next())));
        let inner_content_info = p12::ContentInfo::Data(safe_contents);
        let auth_safe_bytes =
            yasna::construct_der(|w| w.write_sequence_of(|w| inner_content_info.write(w.next())));
        let password_bmp = pkcs12_bmp_string(password);
        let mac_data = p12::MacData::new(&auth_safe_bytes, &password_bmp);
        let pfx = p12::PFX {
            version: 3,
            auth_safe: p12::ContentInfo::Data(auth_safe_bytes),
            mac_data: Some(mac_data),
        };
        pfx.to_der()
    }

    #[test]
    fn pkcs12_null_password_skips_mac_verification() {
        // `KeyStore.load(stream, null)` — a Java `null` char[], not an empty
        // one — must skip PKCS#12 integrity checking entirely, matching
        // real-JDK's PKCS12KeyStore. `WebServerSslBundleTests` relies on this:
        // it opens a keystore's key entries without a keyStorePassword,
        // supplying the entry password later through `getKey()`. Passing an
        // empty password to `load_pkcs12` (its `verify_mac=true` form) must
        // still fail, since that's a real empty-string password attempt, not
        // a null one — only the explicit `verify_mac=false` path skips it.
        let bytes = synth_unencrypted_content_pkcs12("secret");
        let err = load_pkcs12(&bytes, b"").unwrap_err();
        assert!(matches!(err, KeyStoreError::Pkcs12MacFailed));
        load_pkcs12_ex(&bytes, b"", false).expect("null password skips MAC check");
    }

    #[test]
    fn detect_algo_idx_handles_unknown() {
        // No RSA/EC OID — defaults to 6 (RSA).
        assert_eq!(detect_algo_idx(&[0u8; 32]), 6);
    }

    #[test]
    fn detect_algo_idx_finds_rsa_oid() {
        let mut blob = vec![0u8; 16];
        blob.extend_from_slice(&[
            0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01,
        ]);
        assert_eq!(detect_algo_idx(&blob), 6);
    }

    #[test]
    fn detect_algo_idx_finds_ec_oid() {
        let mut blob = vec![0u8; 16];
        blob.extend_from_slice(&[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]);
        assert_eq!(detect_algo_idx(&blob), 7);
    }

    #[test]
    fn fnv1a_32_stable_across_runs() {
        // Compile-time stable, sanity check the constants.
        assert_eq!(fnv1a_32(b""), 0x811C_9DC5);
        // Hash is deterministic per-input.
        let h1 = fnv1a_32(b"hello");
        let h2 = fnv1a_32(b"hello");
        assert_eq!(h1, h2);
        let h3 = fnv1a_32(b"world");
        assert_ne!(h1, h3);
    }

    // -----------------------------------------------------------------
    // PKCS#12 writer (`pkcs12-setentry-secretkeyentry-…-20260805`)
    // -----------------------------------------------------------------

    fn secret_entry(alias: &str, bytes: &[u8], algorithm: &str) -> (String, KeyStoreEntry) {
        (
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: EntryKind::SecretKey {
                    key_bytes: bytes.to_vec(),
                    algorithm: algorithm.to_string(),
                },
            },
        )
    }

    fn cert_entry(alias: &str, der: &[u8]) -> (String, KeyStoreEntry) {
        (
            alias.to_string(),
            KeyStoreEntry {
                alias: alias.to_string(),
                creation_time_ms: 0,
                kind: EntryKind::TrustedCert {
                    cert_der: der.to_vec(),
                },
            },
        )
    }

    fn store_of(entries: Vec<(String, KeyStoreEntry)>) -> LoadedKeyStore {
        let mut map: IndexMap<String, KeyStoreEntry> = IndexMap::new();
        for (alias, entry) in entries {
            map.insert(alias, entry);
        }
        LoadedKeyStore { entries: map }
    }

    /// The defect this file records, reduced to the storage layer: a keystore
    /// holding a secret key had to survive `store` -> `load` with its BYTES,
    /// its ALGORITHM and its ALIAS intact. Asserting only "an entry came back"
    /// would have passed against the "RAW" bug.
    #[test]
    fn pkcs12_round_trips_a_secret_key_with_its_algorithm() {
        let key = [
            0u8, 7, 14, 21, 28, 35, 42, 49, 56, 63, 70, 77, 84, 91, 98, 105,
        ];
        let store = store_of(vec![secret_entry("ske", &key, "AES")]);
        let der = write_pkcs12(&store, b"changeit").expect("write");
        let back = load_pkcs12(&der, b"changeit").expect("load");

        assert_eq!(back.entries.len(), 1, "entry count after round trip");
        let entry = back.entries.get("ske").expect("alias preserved");
        match &entry.kind {
            EntryKind::SecretKey {
                key_bytes,
                algorithm,
            } => {
                assert_eq!(key_bytes.as_slice(), &key, "key material");
                assert_eq!(algorithm, "AES", "algorithm name, not the format");
            }
            other => panic!("secret key came back as {other:?}"),
        }
    }

    /// DESede is the row a guess gets wrong (`1.3.14.3.2.17`, not PKCS#3's
    /// `des-EDE3-CBC`), so it is worth its own assertion rather than trusting
    /// the AES case to cover the table.
    #[test]
    fn pkcs12_round_trips_desede_under_the_oid_the_jdk_uses() {
        let store = store_of(vec![secret_entry("des", &[0x5au8; 24], "DESede")]);
        // The identifier itself sits INSIDE the encrypted SecretBag, so assert
        // it at the table rather than by scanning the ciphertext.
        assert_eq!(
            secret_key_alg_oid("DESede")
                .unwrap()
                .components()
                .as_slice(),
            &[1u64, 3, 14, 3, 2, 17]
        );
        let der = write_pkcs12(&store, b"changeit").expect("write");
        let back = load_pkcs12(&der, b"changeit").expect("load");
        match &back.entries.get("des").expect("alias").kind {
            EntryKind::SecretKey { algorithm, .. } => assert_eq!(algorithm, "DESede"),
            other => panic!("came back as {other:?}"),
        }
    }

    /// A store with all three entry kinds. The private key exercises the
    /// shrouded-key-bag + localKeyId pairing, the trusted cert exercises the
    /// `trustedKeyUsage` attribute, and the secret key is the reason the whole
    /// writer exists.
    #[test]
    fn pkcs12_round_trips_a_mixed_store() {
        let leaf = b"\x30\x0aLEAFCERT01".to_vec();
        let mut entries = IndexMap::new();
        entries.insert(
            "pk".to_string(),
            KeyStoreEntry {
                alias: "pk".to_string(),
                creation_time_ms: 0,
                kind: EntryKind::PrivateKey {
                    key_der: b"\x30\x08PRIVKEY1".to_vec(),
                    chain: vec![leaf.clone()],
                },
            },
        );
        let (a, e) = cert_entry("ca", b"\x30\x08CACERT01");
        entries.insert(a, e);
        let (a, e) = secret_entry("sk", &[1u8, 2, 3, 4], "HmacSHA256");
        entries.insert(a, e);
        let store = LoadedKeyStore { entries };

        let der = write_pkcs12(&store, b"pw").expect("write");
        let back = load_pkcs12(&der, b"pw").expect("load");
        assert_eq!(back.entries.len(), 3, "aliases: {:?}", back.entries.keys());

        match &back.entries.get("pk").expect("pk").kind {
            EntryKind::PrivateKey { key_der, chain } => {
                assert_eq!(key_der.as_slice(), b"\x30\x08PRIVKEY1");
                assert_eq!(chain, &vec![leaf]);
            }
            other => panic!("pk came back as {other:?}"),
        }
        match &back.entries.get("ca").expect("ca").kind {
            EntryKind::TrustedCert { cert_der } => {
                assert_eq!(cert_der.as_slice(), b"\x30\x08CACERT01")
            }
            other => panic!("ca came back as {other:?}"),
        }
        match &back.entries.get("sk").expect("sk").kind {
            EntryKind::SecretKey {
                key_bytes,
                algorithm,
            } => {
                assert_eq!(key_bytes.as_slice(), &[1u8, 2, 3, 4]);
                assert_eq!(algorithm, "HmacSHA256");
            }
            other => panic!("sk came back as {other:?}"),
        }
    }

    /// The MAC is not decoration: a wrong password must be rejected rather
    /// than yielding an empty-looking keystore.
    #[test]
    fn pkcs12_write_produces_a_mac_that_rejects_the_wrong_password() {
        let store = store_of(vec![secret_entry("ske", &[9u8; 16], "AES")]);
        let der = write_pkcs12(&store, b"right").expect("write");
        assert!(load_pkcs12(&der, b"right").is_ok());
        assert!(matches!(
            load_pkcs12(&der, b"wrong"),
            Err(KeyStoreError::Pkcs12MacFailed)
        ));
    }

    /// `write_pkcs12` must not emit a file whose secret key would read back as
    /// a DIFFERENT algorithm, so an unencodable name is an error, not a guess.
    /// The same names a real JDK's `AlgorithmId.get` refuses are refused here.
    #[test]
    fn pkcs12_refuses_a_secret_key_algorithm_it_cannot_encode() {
        let store = store_of(vec![secret_entry("x", &[0u8; 16], "ChaCha20")]);
        let err = write_pkcs12(&store, b"pw").expect_err("must not invent an OID");
        assert!(
            err.contains("ChaCha20"),
            "message names the algorithm: {err}"
        );
    }

    /// A name this VM only READS (`AlgorithmId.getName()` is asymmetric for
    /// three OIDs) and a bare dotted OID both have to encode, or `load` ->
    /// `store` of somebody else's keystore would fail where it used to work.
    #[test]
    fn secret_key_alg_oid_accepts_read_side_names_and_bare_oids() {
        assert!(secret_key_alg_oid("DES/CBC").is_some());
        assert!(secret_key_alg_oid("RC2/CBC/PKCS5Padding").is_some());
        assert!(secret_key_alg_oid("1.2.840.113549.3.7").is_some());
        assert!(
            secret_key_alg_oid("aes").is_some(),
            "names are case-insensitive"
        );
        assert!(secret_key_alg_oid("not an algorithm").is_none());
        assert!(
            secret_key_alg_oid("9.99.1").is_none(),
            "an OID whose first arc is out of range must not reach the DER writer"
        );
    }

    #[test]
    fn secret_key_alg_name_falls_back_to_the_dotted_oid() {
        let unknown = yasna::models::ObjectIdentifier::from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(secret_key_alg_name(&unknown), "1.2.3.4.5");
        let aes = yasna::models::ObjectIdentifier::from_slice(&[2, 16, 840, 1, 101, 3, 4, 1]);
        assert_eq!(secret_key_alg_name(&aes), "AES");
    }

    /// A secret key cannot go into a JKS body at all; it must be the store's
    /// format choice that changes, never the entry that disappears.
    #[test]
    fn jks_still_omits_secret_keys_and_keeps_insertion_order() {
        let mut entries = IndexMap::new();
        let (a, e) = cert_entry("zzz-first", b"\x30\x06DUMMY1");
        entries.insert(a, e);
        let (a, e) = secret_entry("secret", &[1u8; 16], "AES");
        entries.insert(a, e);
        let (a, e) = cert_entry("aaa-second", b"\x30\x06DUMMY2");
        entries.insert(a, e);
        let store = LoadedKeyStore { entries };

        let jks = write_jks(&store, b"pw");
        let back = load_jks(&jks, b"pw").expect("load");
        assert_eq!(
            back.entries.keys().collect::<Vec<_>>(),
            vec!["zzz-first", "aaa-second"],
            "insertion order, not alphabetical, and no secret key"
        );
    }
}

/// `unwrap_keystore_spi`'s slot-2 fallback, both directions.
///
/// # The defect these hold shut
///
/// `KeyStore.setEntry("secret", new SecretKeyEntry(...), new PasswordProtection(pw))`
/// returned normally and stored NOTHING. Measured on JDK 25 in both
/// `--real-jdk` and `--jdk-only`:
///
/// ```text
///                          HotSpot   CratonVM
/// size() after setEntry       1         0
/// containsAlias("secret")   true     false
/// getKey("secret", pw)     <key>      null
/// store(...) byte count      421       122      (an EMPTY keystore)
/// setKeyEntry(...)   -- the pre-1.5 route to the same thing -- worked
/// ```
///
/// `engine_set_entry` was the one `engine*` callback that read its store id
/// through `keystore_ensure_store_id`, which UNWRAPS. Its receiver is already
/// the SPI, and for `KeyStore.getInstance("PKCS12")` that SPI is
/// `sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12`, a
/// `sun/security/util/KeyStoreDelegator`. A delegator has no `keyStoreSpi`
/// field, so the name lookup missed and the blind slot-2 fallback fired --
/// and slot 2 of `KeyStoreDelegator` is `primaryKeyStore`, a `java.lang.Class`
/// MIRROR. Every `setEntry` was therefore filed under the store id of a
/// process-global class mirror, which no reader ever asks for.
///
/// # Why a mock and not a source scan
///
/// A source witness would pin today's spelling of the guard; these call the
/// function. The second test is the one that keeps the first honest: a guard
/// that rejected everything would make the delegator case pass while silently
/// breaking every real `java.security.KeyStore` wrapper, which is the caller
/// the fallback exists for.
#[cfg(test)]
mod unwrap_keystore_spi_tests {
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;
    // The mock implements these through the trait, so the trait has to be in
    // scope for `alloc_object` / `set_field` / `class_id_by_name` to resolve.
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

    /// Slot 2 of the receiver is a `java.lang.Class`, exactly as it is on a
    /// `KeyStoreDelegator`. The unwrap must refuse it and answer the receiver.
    #[test]
    fn a_slot_two_that_is_not_a_keystore_spi_is_not_followed() {
        let mut ctx = MockNativeContext::new();
        let spi_cid = ctx.declare_class("java/security/KeyStoreSpi", &[]);
        let delegator_cid = ctx.declare_class(
            "sun/security/pkcs12/PKCS12KeyStore$DualFormatPKCS12",
            &[
                ("primaryType", "Ljava/lang/String;"),
                ("secondaryType", "Ljava/lang/String;"),
                ("primaryKeyStore", "Ljava/lang/Class;"),
            ],
        );
        let class_mirror_cid = ctx.declare_class("java/lang/Class", &[]);
        assert_ne!(
            spi_cid, class_mirror_cid,
            "the mock handed the same ClassId to two names, so this test could not tell \
             a Class mirror from a KeyStoreSpi and would pass on the broken tree"
        );

        let mirror = ctx.alloc_object(class_mirror_cid, 0);
        let delegator = ctx.alloc_object(delegator_cid, 8);
        ctx.set_field(delegator, 2, Value::Object(Some(mirror)));

        let got = unwrap_keystore_spi(&mut ctx, delegator);
        assert_eq!(
            got, delegator,
            "unwrap_keystore_spi followed slot 2 to a java.lang.Class. That is how \
             KeyStore.setEntry came to store its entries against the store id of a \
             process-global class mirror: setEntry returned normally, threw nothing, and \
             size()/aliases()/getKey() all answered as if it had never been called."
        );
    }

    /// The same fallback, with a slot 2 that IS a `KeyStoreSpi`. It must still
    /// be followed — otherwise the narrowing above has broken the real
    /// `java.security.KeyStore` wrapper it was never aimed at.
    #[test]
    fn a_slot_two_that_is_a_keystore_spi_is_still_followed() {
        let mut ctx = MockNativeContext::new();
        let spi_cid = ctx.declare_class("java/security/KeyStoreSpi", &[]);
        let wrapper_cid = ctx.declare_class(
            "java/security/KeyStore",
            &[
                ("type", "Ljava/lang/String;"),
                ("provider", "Ljava/security/Provider;"),
                ("keyStoreSpi", "Ljava/security/KeyStoreSpi;"),
            ],
        );

        let spi = ctx.alloc_object(spi_cid, 1);
        let wrapper = ctx.alloc_object(wrapper_cid, 4);
        // By INDEX only: the by-name tier is what a real wrapper answers, and
        // this test is about the tier underneath it.
        ctx.set_field(wrapper, 2, Value::Object(Some(spi)));

        let got = unwrap_keystore_spi(&mut ctx, wrapper);
        assert_eq!(
            got, spi,
            "the slot-2 fallback stopped following a genuine KeyStoreSpi. That tier is the \
             only one that resolves a synthetic KeyStore mirror, so breaking it drops every \
             staged keystore identity -- the KeyManagerFactory/TrustManagerFactory defect \
             `keystore_id_from_object` documents."
        );
    }

    /// When `java/security/KeyStoreSpi` cannot be resolved at all, the check
    /// cannot adjudicate and the historical blind behaviour is kept.
    ///
    /// Written down as a test because it is a decision, not an accident: an
    /// image with no such class (a `--synthetic-jdk` mirror) must not have its
    /// unwrap silently changed by a guard that has nothing to compare against.
    #[test]
    fn an_unresolvable_keystore_spi_class_keeps_the_old_behaviour() {
        let mut ctx = MockNativeContext::new();
        // Deliberately NOT declaring `java/security/KeyStoreSpi`.
        let carrier_cid = ctx.declare_class("cratonvm/internal/ks/Carrier", &[]);
        let inner_cid = ctx.declare_class("cratonvm/internal/ks/Inner", &[]);
        let inner = ctx.alloc_object(inner_cid, 1);
        let carrier = ctx.alloc_object(carrier_cid, 6);
        ctx.set_field(carrier, 2, Value::Object(Some(inner)));

        assert!(
            ctx.class_id_by_name("java/security/KeyStoreSpi").is_none(),
            "this test's premise is that the class is unresolvable; the mock resolved it, \
             so the branch under test was never reached"
        );
        assert_eq!(
            unwrap_keystore_spi(&mut ctx, carrier),
            inner,
            "with no KeyStoreSpi to compare against, the fallback must behave as it always \
             did rather than start refusing on an image that cannot answer the question"
        );
    }
}
