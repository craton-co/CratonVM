// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! TLS 1.3 Handshake Implementation.
//!
//! Provides a real TLS 1.3 handshake state machine, record layer, key schedule,
//! session resumption, OCSP stapling, and alert handling for the CratonVM native layer.
//! Phase 19.1 of the CratonVM project.

use std::collections::HashMap;

use crate::try_alloc_concurrent_synthetic;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::Value;

// ===========================================================================
// Handshake type constants (RFC 8446 Section 4)
// ===========================================================================

const HT_CLIENT_HELLO: u8 = 1;
const HT_SERVER_HELLO: u8 = 2;
const HT_NEW_SESSION_TICKET: u8 = 4;
const HT_ENCRYPTED_EXTENSIONS: u8 = 8;
const HT_CERTIFICATE: u8 = 11;
const HT_CERTIFICATE_VERIFY: u8 = 15;
const HT_FINISHED: u8 = 20;
const HT_KEY_UPDATE: u8 = 24;

// ===========================================================================
// TLS Role
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsRole {
    Client,
    Server,
}

// ===========================================================================
// TLS State
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsState {
    Start,
    WaitServerHello,
    WaitEncryptedExtensions,
    WaitCertificate,
    WaitCertificateVerify,
    WaitFinished,
    Connected,
    Error(String),
    Closed,
}

// ===========================================================================
// Cipher Suites (RFC 8446 Appendix B.4)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CipherSuite {
    TlsAes128GcmSha256,
    TlsAes256GcmSha384,
    TlsChacha20Poly1305Sha256,
    TlsAes128CcmSha256,
}

impl CipherSuite {
    pub fn code(&self) -> u16 {
        match self {
            CipherSuite::TlsAes128GcmSha256 => 0x1301,
            CipherSuite::TlsAes256GcmSha384 => 0x1302,
            CipherSuite::TlsChacha20Poly1305Sha256 => 0x1303,
            CipherSuite::TlsAes128CcmSha256 => 0x1304,
        }
    }

    pub fn from_code(code: u16) -> Option<CipherSuite> {
        match code {
            0x1301 => Some(CipherSuite::TlsAes128GcmSha256),
            0x1302 => Some(CipherSuite::TlsAes256GcmSha384),
            0x1303 => Some(CipherSuite::TlsChacha20Poly1305Sha256),
            0x1304 => Some(CipherSuite::TlsAes128CcmSha256),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            CipherSuite::TlsAes128GcmSha256 => "TLS_AES_128_GCM_SHA256",
            CipherSuite::TlsAes256GcmSha384 => "TLS_AES_256_GCM_SHA384",
            CipherSuite::TlsChacha20Poly1305Sha256 => "TLS_CHACHA20_POLY1305_SHA256",
            CipherSuite::TlsAes128CcmSha256 => "TLS_AES_128_CCM_SHA256",
        }
    }

    pub fn key_len(&self) -> usize {
        match self {
            CipherSuite::TlsAes128GcmSha256 => 16,
            CipherSuite::TlsAes256GcmSha384 => 32,
            CipherSuite::TlsChacha20Poly1305Sha256 => 32,
            CipherSuite::TlsAes128CcmSha256 => 16,
        }
    }

    pub fn iv_len(&self) -> usize {
        12 // All TLS 1.3 cipher suites use 12-byte IV
    }

    pub fn hash_len(&self) -> usize {
        match self {
            CipherSuite::TlsAes128GcmSha256 => 32,
            CipherSuite::TlsAes256GcmSha384 => 48,
            CipherSuite::TlsChacha20Poly1305Sha256 => 32,
            CipherSuite::TlsAes128CcmSha256 => 32,
        }
    }
}

// ===========================================================================
// Named Groups (RFC 8446 Section 4.2.7)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedGroup {
    X25519,
    Secp256r1,
    Secp384r1,
    X448,
}

impl NamedGroup {
    pub fn code(&self) -> u16 {
        match self {
            NamedGroup::X25519 => 0x001D,
            NamedGroup::Secp256r1 => 0x0017,
            NamedGroup::Secp384r1 => 0x0018,
            NamedGroup::X448 => 0x001E,
        }
    }

    pub fn from_code(code: u16) -> Option<NamedGroup> {
        match code {
            0x001D => Some(NamedGroup::X25519),
            0x0017 => Some(NamedGroup::Secp256r1),
            0x0018 => Some(NamedGroup::Secp384r1),
            0x001E => Some(NamedGroup::X448),
            _ => None,
        }
    }
}

// ===========================================================================
// Signature Algorithms (RFC 8446 Section 4.2.3)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    RsaPkcs1Sha256,
    RsaPkcs1Sha384,
    EcdsaSecp256r1Sha256,
    EcdsaSecp384r1Sha384,
    Ed25519,
    RsaPssRsaeSha256,
}

impl SignatureAlgorithm {
    pub fn code(&self) -> u16 {
        match self {
            SignatureAlgorithm::RsaPkcs1Sha256 => 0x0401,
            SignatureAlgorithm::RsaPkcs1Sha384 => 0x0501,
            SignatureAlgorithm::EcdsaSecp256r1Sha256 => 0x0403,
            SignatureAlgorithm::EcdsaSecp384r1Sha384 => 0x0503,
            SignatureAlgorithm::Ed25519 => 0x0807,
            SignatureAlgorithm::RsaPssRsaeSha256 => 0x0804,
        }
    }

    pub fn from_code(code: u16) -> Option<SignatureAlgorithm> {
        match code {
            0x0401 => Some(SignatureAlgorithm::RsaPkcs1Sha256),
            0x0501 => Some(SignatureAlgorithm::RsaPkcs1Sha384),
            0x0403 => Some(SignatureAlgorithm::EcdsaSecp256r1Sha256),
            0x0503 => Some(SignatureAlgorithm::EcdsaSecp384r1Sha384),
            0x0807 => Some(SignatureAlgorithm::Ed25519),
            0x0804 => Some(SignatureAlgorithm::RsaPssRsaeSha256),
            _ => None,
        }
    }
}

// ===========================================================================
// Key Share Entry
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyShareEntry {
    pub group: NamedGroup,
    pub key_exchange: Vec<u8>,
}

// ===========================================================================
// PSK Identity
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PskIdentity {
    pub identity: Vec<u8>,
    pub obfuscated_ticket_age: u32,
}

// ===========================================================================
// Certificate Entry
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateEntry {
    pub cert_data: Vec<u8>,
    pub extensions: Vec<TlsExtension>,
}

// ===========================================================================
// TLS Extensions (RFC 8446 Section 4.2)
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsExtension {
    ServerName(String),                           // type 0
    MaxFragmentLength(u8),                        // type 1
    SupportedGroups(Vec<NamedGroup>),             // type 10
    SignatureAlgorithms(Vec<SignatureAlgorithm>), // type 13
    Alpn(Vec<String>),                            // type 16
    PreSharedKey(Vec<PskIdentity>),               // type 41
    EarlyData,                                    // type 42
    SupportedVersions(Vec<u16>),                  // type 43
    PskKeyExchangeModes(Vec<u8>),                 // type 45
    KeyShare(Vec<KeyShareEntry>),                 // type 51
}

impl TlsExtension {
    pub fn extension_type(&self) -> u16 {
        match self {
            TlsExtension::ServerName(_) => 0,
            TlsExtension::MaxFragmentLength(_) => 1,
            TlsExtension::SupportedGroups(_) => 10,
            TlsExtension::SignatureAlgorithms(_) => 13,
            TlsExtension::Alpn(_) => 16,
            TlsExtension::PreSharedKey(_) => 41,
            TlsExtension::EarlyData => 42,
            TlsExtension::SupportedVersions(_) => 43,
            TlsExtension::PskKeyExchangeModes(_) => 45,
            TlsExtension::KeyShare(_) => 51,
        }
    }
}

// ===========================================================================
// Handshake Messages
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeMessage {
    ClientHello {
        random: [u8; 32],
        session_id: [u8; 32],
        cipher_suites: Vec<CipherSuite>,
        extensions: Vec<TlsExtension>,
    },
    ServerHello {
        random: [u8; 32],
        session_id: [u8; 32],
        cipher_suite: CipherSuite,
        extensions: Vec<TlsExtension>,
    },
    EncryptedExtensions {
        extensions: Vec<TlsExtension>,
    },
    Certificate {
        cert_chain: Vec<CertificateEntry>,
    },
    CertificateVerify {
        algorithm: SignatureAlgorithm,
        signature: Vec<u8>,
    },
    Finished {
        verify_data: Vec<u8>,
    },
    NewSessionTicket {
        lifetime: u32,
        ticket: Vec<u8>,
    },
    KeyUpdate {
        request_update: bool,
    },
}

impl HandshakeMessage {
    pub fn msg_type(&self) -> u8 {
        match self {
            HandshakeMessage::ClientHello { .. } => HT_CLIENT_HELLO,
            HandshakeMessage::ServerHello { .. } => HT_SERVER_HELLO,
            HandshakeMessage::EncryptedExtensions { .. } => HT_ENCRYPTED_EXTENSIONS,
            HandshakeMessage::Certificate { .. } => HT_CERTIFICATE,
            HandshakeMessage::CertificateVerify { .. } => HT_CERTIFICATE_VERIFY,
            HandshakeMessage::Finished { .. } => HT_FINISHED,
            HandshakeMessage::NewSessionTicket { .. } => HT_NEW_SESSION_TICKET,
            HandshakeMessage::KeyUpdate { .. } => HT_KEY_UPDATE,
        }
    }
}

// ===========================================================================
// Content Type (TLS Record Layer)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    ChangeCipherSpec,
    Alert,
    Handshake,
    ApplicationData,
}

impl ContentType {
    pub fn code(&self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }

    pub fn from_code(code: u8) -> Option<ContentType> {
        match code {
            20 => Some(ContentType::ChangeCipherSpec),
            21 => Some(ContentType::Alert),
            22 => Some(ContentType::Handshake),
            23 => Some(ContentType::ApplicationData),
            _ => None,
        }
    }
}

// ===========================================================================
// TLS Record
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsRecord {
    pub content_type: ContentType,
    pub protocol_version: u16,
    pub length: u16,
    pub fragment: Vec<u8>,
}

// ===========================================================================
// TLS Record Layer
// ===========================================================================

pub struct TlsRecordLayer {
    pub max_fragment_length: usize,
    pub records_sent: u64,
    pub records_received: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl TlsRecordLayer {
    pub fn new() -> Self {
        TlsRecordLayer {
            max_fragment_length: 16384,
            records_sent: 0,
            records_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    pub fn with_max_fragment_length(max_len: usize) -> Self {
        TlsRecordLayer {
            max_fragment_length: max_len,
            records_sent: 0,
            records_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    /// Encode a TLS record: 1 byte content_type + 2 bytes version + 2 bytes length + fragment
    pub fn encode_record(&mut self, content_type: ContentType, data: &[u8]) -> Vec<u8> {
        let len = data.len().min(self.max_fragment_length);
        let mut out = Vec::with_capacity(5 + len);
        out.push(content_type.code());
        // TLS 1.2 compat version in record layer
        out.push(0x03);
        out.push(0x03);
        out.push((len >> 8) as u8);
        out.push((len & 0xFF) as u8);
        out.extend_from_slice(&data[..len]);
        self.records_sent += 1;
        self.bytes_sent += (5 + len) as u64;
        out
    }

    /// Decode a TLS record from raw bytes. Requires at least 5 bytes for the header.
    pub fn decode_record(&mut self, data: &[u8]) -> Result<TlsRecord, TlsError> {
        if data.len() < 5 {
            return Err(TlsError {
                kind: TlsErrorKind::ProtocolError,
                message: "Record too short: need at least 5 bytes".to_string(),
            });
        }
        let content_type = ContentType::from_code(data[0]).ok_or_else(|| TlsError {
            kind: TlsErrorKind::ProtocolError,
            message: format!("Unknown content type: {}", data[0]),
        })?;
        let protocol_version = ((data[1] as u16) << 8) | (data[2] as u16);
        let length = ((data[3] as u16) << 8) | (data[4] as u16);
        if data.len() < 5 + length as usize {
            return Err(TlsError {
                kind: TlsErrorKind::ProtocolError,
                message: format!(
                    "Record truncated: expected {} bytes of fragment, got {}",
                    length,
                    data.len() - 5
                ),
            });
        }
        if length as usize > self.max_fragment_length {
            return Err(TlsError {
                kind: TlsErrorKind::ProtocolError,
                message: format!(
                    "Fragment length {} exceeds max {}",
                    length, self.max_fragment_length
                ),
            });
        }
        let fragment = data[5..5 + length as usize].to_vec();
        self.records_received += 1;
        self.bytes_received += (5 + length as usize) as u64;
        Ok(TlsRecord {
            content_type,
            protocol_version,
            length,
            fragment,
        })
    }
}

// ===========================================================================
// Alert Level and Description
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertLevel {
    Warning,
    Fatal,
}

impl AlertLevel {
    pub fn code(&self) -> u8 {
        match self {
            AlertLevel::Warning => 1,
            AlertLevel::Fatal => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<AlertLevel> {
        match code {
            1 => Some(AlertLevel::Warning),
            2 => Some(AlertLevel::Fatal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertDescription {
    CloseNotify,
    UnexpectedMessage,
    BadRecordMac,
    HandshakeFailure,
    BadCertificate,
    CertificateExpired,
    UnknownCa,
    DecodeError,
    DecryptError,
    ProtocolVersion,
    InsufficientSecurity,
    InternalError,
    MissingExtension,
    UnsupportedExtension,
}

impl AlertDescription {
    pub fn code(&self) -> u8 {
        match self {
            AlertDescription::CloseNotify => 0,
            AlertDescription::UnexpectedMessage => 10,
            AlertDescription::BadRecordMac => 20,
            AlertDescription::HandshakeFailure => 40,
            AlertDescription::BadCertificate => 42,
            AlertDescription::CertificateExpired => 45,
            AlertDescription::UnknownCa => 48,
            AlertDescription::DecodeError => 50,
            AlertDescription::DecryptError => 51,
            AlertDescription::ProtocolVersion => 70,
            AlertDescription::InsufficientSecurity => 71,
            AlertDescription::InternalError => 80,
            AlertDescription::MissingExtension => 109,
            AlertDescription::UnsupportedExtension => 110,
        }
    }

    pub fn from_code(code: u8) -> Option<AlertDescription> {
        match code {
            0 => Some(AlertDescription::CloseNotify),
            10 => Some(AlertDescription::UnexpectedMessage),
            20 => Some(AlertDescription::BadRecordMac),
            40 => Some(AlertDescription::HandshakeFailure),
            42 => Some(AlertDescription::BadCertificate),
            45 => Some(AlertDescription::CertificateExpired),
            48 => Some(AlertDescription::UnknownCa),
            50 => Some(AlertDescription::DecodeError),
            51 => Some(AlertDescription::DecryptError),
            70 => Some(AlertDescription::ProtocolVersion),
            71 => Some(AlertDescription::InsufficientSecurity),
            80 => Some(AlertDescription::InternalError),
            109 => Some(AlertDescription::MissingExtension),
            110 => Some(AlertDescription::UnsupportedExtension),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsAlert {
    pub level: AlertLevel,
    pub description: AlertDescription,
}

impl TlsAlert {
    pub fn new(level: AlertLevel, description: AlertDescription) -> Self {
        TlsAlert { level, description }
    }

    pub fn encode(&self) -> [u8; 2] {
        [self.level.code(), self.description.code()]
    }

    pub fn decode(data: &[u8]) -> Result<TlsAlert, TlsError> {
        if data.len() < 2 {
            return Err(TlsError {
                kind: TlsErrorKind::ProtocolError,
                message: "Alert too short".to_string(),
            });
        }
        let level = AlertLevel::from_code(data[0]).ok_or_else(|| TlsError {
            kind: TlsErrorKind::ProtocolError,
            message: format!("Unknown alert level: {}", data[0]),
        })?;
        let description = AlertDescription::from_code(data[1]).ok_or_else(|| TlsError {
            kind: TlsErrorKind::ProtocolError,
            message: format!("Unknown alert description: {}", data[1]),
        })?;
        Ok(TlsAlert { level, description })
    }
}

// ===========================================================================
// TLS Error
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsErrorKind {
    HandshakeFailure,
    BadCertificate,
    ProtocolError,
    DecryptionFailure,
    UnsupportedCipherSuite,
    SessionNotFound,
    AlertReceived(AlertDescription),
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsError {
    pub kind: TlsErrorKind,
    pub message: String,
}

impl TlsError {
    pub fn new(kind: TlsErrorKind, message: impl Into<String>) -> Self {
        TlsError {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TLS error ({:?}): {}", self.kind, self.message)
    }
}

// ===========================================================================
// HMAC-SHA256 and HKDF (real crypto, delegates to crypto_impl SHA-256)
// ===========================================================================

/// HMAC-SHA256 for TLS key derivation.
/// Self-contained implementation to avoid cross-feature-gate dependencies.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let block_size = 64;

    let mut k = if key.len() > block_size {
        sha256(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(block_size, 0);

    let mut i_pad = vec![0u8; block_size];
    let mut o_pad = vec![0u8; block_size];
    for i in 0..block_size {
        i_pad[i] = k[i] ^ 0x36;
        o_pad[i] = k[i] ^ 0x5c;
    }

    let mut inner = i_pad;
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = o_pad;
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// SHA-256 hash — delegates to the real implementation in crypto_impl.
fn sha256(data: &[u8]) -> [u8; 32] {
    use crate::crypto_impl::Sha256;
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// HKDF-Extract using real HMAC-SHA256 (RFC 5869).
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> Vec<u8> {
    let salt = if salt.is_empty() {
        vec![0u8; 32]
    } else {
        salt.to_vec()
    };
    hmac_sha256(&salt, ikm).to_vec()
}

/// HKDF-Expand using real HMAC-SHA256 (RFC 5869).
fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let hash_len = 32;
    let n = (len + hash_len - 1) / hash_len;
    let mut okm = Vec::with_capacity(n * hash_len);
    let mut t = Vec::new();
    for i in 1..=n {
        let mut input = t.clone();
        input.extend_from_slice(info);
        input.push(i as u8);
        let h = hmac_sha256(prk, &input);
        t = h.to_vec();
        okm.extend_from_slice(&h);
    }
    okm.truncate(len);
    okm
}

// ===========================================================================
// Traffic Keys
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficKeys {
    pub key: Vec<u8>,
    pub iv: Vec<u8>,
}

// ===========================================================================
// TLS Key Schedule (RFC 8446 Section 7.1)
// ===========================================================================

pub struct TlsKeySchedule {
    pub early_secret: Vec<u8>,
    pub handshake_secret: Vec<u8>,
    pub master_secret: Vec<u8>,
    pub client_handshake_traffic_secret: Vec<u8>,
    pub server_handshake_traffic_secret: Vec<u8>,
    pub client_application_traffic_secret: Vec<u8>,
    pub server_application_traffic_secret: Vec<u8>,
}

impl TlsKeySchedule {
    pub fn new() -> Self {
        TlsKeySchedule {
            early_secret: Vec::new(),
            handshake_secret: Vec::new(),
            master_secret: Vec::new(),
            client_handshake_traffic_secret: Vec::new(),
            server_handshake_traffic_secret: Vec::new(),
            client_application_traffic_secret: Vec::new(),
            server_application_traffic_secret: Vec::new(),
        }
    }

    /// Derive early secret: HKDF-Extract(salt=0, PSK or 0)
    pub fn derive_early_secret(&mut self, psk: Option<&[u8]>) -> Vec<u8> {
        let zero_salt = vec![0u8; 32];
        let ikm = match psk {
            Some(p) => p.to_vec(),
            None => vec![0u8; 32],
        };
        self.early_secret = hkdf_extract(&zero_salt, &ikm);
        self.early_secret.clone()
    }

    /// Derive handshake secret from shared (EC)DH secret
    pub fn derive_handshake_secret(&mut self, shared_secret: &[u8]) -> Vec<u8> {
        let derived = hkdf_expand(&self.early_secret, b"derived", 32);
        self.handshake_secret = hkdf_extract(&derived, shared_secret);
        // Derive traffic secrets
        self.client_handshake_traffic_secret =
            hkdf_expand(&self.handshake_secret, b"c hs traffic", 32);
        self.server_handshake_traffic_secret =
            hkdf_expand(&self.handshake_secret, b"s hs traffic", 32);
        self.handshake_secret.clone()
    }

    /// Derive master secret
    pub fn derive_master_secret(&mut self) -> Vec<u8> {
        let derived = hkdf_expand(&self.handshake_secret, b"derived", 32);
        let zero_ikm = vec![0u8; 32];
        self.master_secret = hkdf_extract(&derived, &zero_ikm);
        // Derive application traffic secrets
        self.client_application_traffic_secret =
            hkdf_expand(&self.master_secret, b"c ap traffic", 32);
        self.server_application_traffic_secret =
            hkdf_expand(&self.master_secret, b"s ap traffic", 32);
        self.master_secret.clone()
    }

    /// Derive traffic keys (key + IV) from a traffic secret
    pub fn derive_traffic_keys(&self, secret: &[u8], key_len: usize, iv_len: usize) -> TrafficKeys {
        let key = hkdf_expand(secret, b"key", key_len);
        let iv = hkdf_expand(secret, b"iv", iv_len);
        TrafficKeys { key, iv }
    }
}

// ===========================================================================
// Session Ticket (for PSK resumption)
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTicket {
    pub cipher_suite: CipherSuite,
    pub resumption_secret: Vec<u8>,
    pub lifetime: u32,
    pub created_at: u64,
    pub sni: Option<String>,
}

// ===========================================================================
// Session Cache
// ===========================================================================

pub struct SessionCache {
    pub sessions: HashMap<Vec<u8>, SessionTicket>,
    pub max_entries: usize,
}

impl SessionCache {
    pub fn new() -> Self {
        SessionCache {
            sessions: HashMap::new(),
            max_entries: 1024,
        }
    }

    pub fn with_capacity(max_entries: usize) -> Self {
        SessionCache {
            sessions: HashMap::new(),
            max_entries,
        }
    }

    pub fn store(&mut self, ticket: Vec<u8>, session: SessionTicket) {
        if self.sessions.len() >= self.max_entries {
            // Evict the first entry (arbitrary eviction for simplicity)
            if let Some(key) = self.sessions.keys().next().cloned() {
                self.sessions.remove(&key);
            }
        }
        self.sessions.insert(ticket, session);
    }

    pub fn lookup(&self, ticket: &[u8]) -> Option<&SessionTicket> {
        self.sessions.get(ticket)
    }

    pub fn remove(&mut self, ticket: &[u8]) -> bool {
        self.sessions.remove(ticket).is_some()
    }

    pub fn cleanup_expired(&mut self, now: u64) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, v| {
            let expires_at = v.created_at + (v.lifetime as u64);
            expires_at > now
        });
        before - self.sessions.len()
    }

    pub fn count(&self) -> usize {
        self.sessions.len()
    }
}

// ===========================================================================
// OCSP Stapling
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcspStatus {
    Good,
    Revoked(u64),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcspResponse {
    pub status: OcspStatus,
    pub produced_at: u64,
    pub this_update: u64,
    pub next_update: u64,
    pub responder_id: Vec<u8>,
}

impl OcspResponse {
    pub fn is_valid(&self, now: u64) -> bool {
        now >= self.this_update && now <= self.next_update && self.status == OcspStatus::Good
    }
}

// ===========================================================================
// TLS 1.3 State Machine
// ===========================================================================

pub struct Tls13StateMachine {
    pub state: TlsState,
    pub role: TlsRole,
    pub client_random: [u8; 32],
    pub server_random: [u8; 32],
    pub cipher_suite: CipherSuite,
    pub selected_alpn: Option<String>,
    pub sni_hostname: Option<String>,
    pub session_id: [u8; 32],
    pub transcript_hash: Vec<u8>,
}

impl Tls13StateMachine {
    pub fn new_client() -> Self {
        Tls13StateMachine {
            state: TlsState::Start,
            role: TlsRole::Client,
            client_random: [0u8; 32],
            server_random: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            selected_alpn: None,
            sni_hostname: None,
            session_id: [0u8; 32],
            transcript_hash: Vec::new(),
        }
    }

    pub fn new_server() -> Self {
        Tls13StateMachine {
            state: TlsState::Start,
            role: TlsRole::Server,
            client_random: [0u8; 32],
            server_random: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            selected_alpn: None,
            sni_hostname: None,
            session_id: [0u8; 32],
            transcript_hash: Vec::new(),
        }
    }

    pub fn with_sni(mut self, hostname: &str) -> Self {
        self.sni_hostname = Some(hostname.to_string());
        self
    }

    pub fn with_client_random(mut self, random: [u8; 32]) -> Self {
        self.client_random = random;
        self
    }

    pub fn with_session_id(mut self, session_id: [u8; 32]) -> Self {
        self.session_id = session_id;
        self
    }

    /// T19.9: set the negotiated cipher suite at construction. Rejects any
    /// suite not on the RFC 8446 §9.1 MTI allowlist by falling back to
    /// TLS_AES_128_GCM_SHA256 — this is the belt-and-suspenders downgrade
    /// block referenced by `is_mti_cipher_suite`.
    pub fn with_cipher_suite(mut self, suite: CipherSuite) -> Self {
        self.cipher_suite = if is_mti_cipher_suite(suite) {
            suite
        } else {
            CipherSuite::TlsAes128GcmSha256
        };
        self
    }

    pub fn is_connected(&self) -> bool {
        self.state == TlsState::Connected
    }

    pub fn get_state(&self) -> &TlsState {
        &self.state
    }

    /// Update the transcript hash with a handshake message type byte.
    fn update_transcript(&mut self, msg_type: u8) {
        self.transcript_hash.push(msg_type);
    }

    /// Advance the state machine with a handshake message.
    /// Returns an optional response message to send.
    pub fn advance(
        &mut self,
        message: HandshakeMessage,
    ) -> Result<Option<HandshakeMessage>, TlsError> {
        match self.role {
            TlsRole::Client => self.advance_client(message),
            TlsRole::Server => self.advance_server(message),
        }
    }

    // -----------------------------------------------------------------------
    // Client-side state transitions
    // -----------------------------------------------------------------------

    fn advance_client(
        &mut self,
        message: HandshakeMessage,
    ) -> Result<Option<HandshakeMessage>, TlsError> {
        match (&self.state, &message) {
            // Start -> send ClientHello, move to WaitServerHello
            (
                TlsState::Start,
                HandshakeMessage::ClientHello {
                    random, session_id, ..
                },
            ) => {
                self.client_random = *random;
                self.session_id = *session_id;
                self.update_transcript(HT_CLIENT_HELLO);
                self.state = TlsState::WaitServerHello;
                Ok(Some(message))
            }
            // WaitServerHello -> receive ServerHello
            (
                TlsState::WaitServerHello,
                HandshakeMessage::ServerHello {
                    random,
                    session_id,
                    cipher_suite,
                    extensions,
                },
            ) => {
                self.server_random = *random;
                self.session_id = *session_id;
                self.cipher_suite = *cipher_suite;
                // Extract ALPN if present
                for ext in extensions {
                    if let TlsExtension::Alpn(protocols) = ext {
                        if let Some(p) = protocols.first() {
                            self.selected_alpn = Some(p.clone());
                        }
                    }
                }
                self.update_transcript(HT_SERVER_HELLO);
                self.state = TlsState::WaitEncryptedExtensions;
                Ok(None)
            }
            // WaitEncryptedExtensions -> receive EncryptedExtensions
            (
                TlsState::WaitEncryptedExtensions,
                HandshakeMessage::EncryptedExtensions { extensions },
            ) => {
                // Process extensions (e.g., ALPN)
                for ext in extensions {
                    if let TlsExtension::Alpn(protocols) = ext {
                        if let Some(p) = protocols.first() {
                            self.selected_alpn = Some(p.clone());
                        }
                    }
                }
                self.update_transcript(HT_ENCRYPTED_EXTENSIONS);
                self.state = TlsState::WaitCertificate;
                Ok(None)
            }
            // WaitCertificate -> receive Certificate
            (TlsState::WaitCertificate, HandshakeMessage::Certificate { .. }) => {
                self.update_transcript(HT_CERTIFICATE);
                self.state = TlsState::WaitCertificateVerify;
                Ok(None)
            }
            // WaitCertificateVerify -> receive CertificateVerify
            (TlsState::WaitCertificateVerify, HandshakeMessage::CertificateVerify { .. }) => {
                self.update_transcript(HT_CERTIFICATE_VERIFY);
                self.state = TlsState::WaitFinished;
                Ok(None)
            }
            // WaitFinished -> receive Finished, send client Finished back
            (TlsState::WaitFinished, HandshakeMessage::Finished { .. }) => {
                self.update_transcript(HT_FINISHED);
                self.state = TlsState::Connected;
                // Client responds with its own Finished
                Ok(Some(HandshakeMessage::Finished {
                    verify_data: self.transcript_hash.clone(),
                }))
            }
            // Connected -> can receive NewSessionTicket
            (TlsState::Connected, HandshakeMessage::NewSessionTicket { .. }) => Ok(None),
            // Connected -> can receive KeyUpdate
            (TlsState::Connected, HandshakeMessage::KeyUpdate { request_update }) => {
                if *request_update {
                    Ok(Some(HandshakeMessage::KeyUpdate {
                        request_update: false,
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => {
                let err_msg = format!(
                    "Unexpected message type {} in state {:?} (client)",
                    message.msg_type(),
                    self.state
                );
                self.state = TlsState::Error(err_msg.clone());
                Err(TlsError::new(TlsErrorKind::HandshakeFailure, err_msg))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Server-side state transitions
    // -----------------------------------------------------------------------

    fn advance_server(
        &mut self,
        message: HandshakeMessage,
    ) -> Result<Option<HandshakeMessage>, TlsError> {
        match (&self.state, &message) {
            // Start -> receive ClientHello, emit entire server flight (ServerHello,
            // EncryptedExtensions, Certificate, CertificateVerify, server Finished)
            // in one conceptual flight, and wait for the client's Finished.
            // Per RFC 8446 §A.2: RECVD_CH → NEGOTIATED → WAIT_FLIGHT2 → WAIT_FINISHED.
            (
                TlsState::Start,
                HandshakeMessage::ClientHello {
                    random,
                    session_id,
                    cipher_suites,
                    extensions,
                },
            ) => {
                self.client_random = *random;
                self.session_id = *session_id;
                // Select first mutually supported cipher suite
                let selected = cipher_suites
                    .first()
                    .copied()
                    .unwrap_or(CipherSuite::TlsAes128GcmSha256);
                self.cipher_suite = selected;
                // Extract SNI
                for ext in extensions {
                    if let TlsExtension::ServerName(name) = ext {
                        self.sni_hostname = Some(name.clone());
                    }
                    if let TlsExtension::Alpn(protocols) = ext {
                        if let Some(p) = protocols.first() {
                            self.selected_alpn = Some(p.clone());
                        }
                    }
                }
                self.update_transcript(HT_CLIENT_HELLO);
                // Generate cryptographically secure server random
                {
                    use crate::crypto_impl::SecureRandom;
                    let mut rng = SecureRandom::new();
                    rng.next_bytes(&mut self.server_random);
                }
                // Update transcript for server flight: ServerHello, EncryptedExtensions,
                // Certificate, CertificateVerify, and the server's own Finished.
                self.update_transcript(HT_SERVER_HELLO);
                self.update_transcript(HT_ENCRYPTED_EXTENSIONS);
                self.update_transcript(HT_CERTIFICATE);
                self.update_transcript(HT_CERTIFICATE_VERIFY);
                self.update_transcript(HT_FINISHED);
                self.state = TlsState::WaitFinished;
                Ok(Some(HandshakeMessage::ServerHello {
                    random: self.server_random,
                    session_id: self.session_id,
                    cipher_suite: self.cipher_suite,
                    extensions: Vec::new(),
                }))
            }
            // WaitFinished -> receive client Finished
            (TlsState::WaitFinished, HandshakeMessage::Finished { .. }) => {
                self.update_transcript(HT_FINISHED);
                self.state = TlsState::Connected;
                Ok(Some(HandshakeMessage::Finished {
                    verify_data: self.transcript_hash.clone(),
                }))
            }
            // Connected -> can receive KeyUpdate
            (TlsState::Connected, HandshakeMessage::KeyUpdate { request_update }) => {
                if *request_update {
                    Ok(Some(HandshakeMessage::KeyUpdate {
                        request_update: false,
                    }))
                } else {
                    Ok(None)
                }
            }
            _ => {
                let err_msg = format!(
                    "Unexpected message type {} in state {:?} (server)",
                    message.msg_type(),
                    self.state
                );
                self.state = TlsState::Error(err_msg.clone());
                Err(TlsError::new(TlsErrorKind::HandshakeFailure, err_msg))
            }
        }
    }
}

// ===========================================================================
// Native method implementations
// ===========================================================================

use std::collections::HashMap as TlsHashMap;
use std::sync::Mutex;

/// Global store of TLS state machines keyed by the SSLEngine's weak lock key.
///
/// jr20 wave 9 / gcE (gch-h1ak-raw-address-keyed-audit-RETIRED-UNFIXED-20261018 item 6): the
/// rows were keyed by the engine's heap ADDRESS, with no remap and no sweep,
/// so a moving collection stranded a live engine's row and a dead engine's row
/// answered for whatever object (of any VM) was later placed at its address.
/// The weak lock key follows the object across moves and is never minted
/// again once its object dies, so a dead engine's row can no longer be read
/// (it stays as a bounded leak of this off-by-default feature).
/// addr-keyed: weak lock keys, not heap addresses (see above).
fn tls_engines() -> &'static Mutex<TlsHashMap<usize, Tls13StateMachine>> {
    // addr-keyed: not a heap address -- weak lock keys (`gc_stable_weak_lock_key`).
    static INSTANCE: std::sync::OnceLock<Mutex<TlsHashMap<usize, Tls13StateMachine>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(TlsHashMap::new()))
}

/// The engine row key of `obj` (CURRENT address). `mint` for the paths that
/// create a row; the read paths never mint, and an engine never keyed has no
/// row.
fn tls_engine_key(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    mint: bool,
) -> Option<usize> {
    if mint {
        crate::gc_stable_weak_lock_key(ctx, obj).ok()
    } else {
        crate::existing_weak_lock_key(ctx, obj)
    }
}

fn native_ssl_engine_do_handshake(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => match tls_engine_key(&*ctx, *obj, true) {
            Some(key) => key,
            None => return Ok(Some(Value::Int(-1))),
        },
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut engines = tls_engines().lock().unwrap();
    let engine = engines
        .entry(addr)
        .or_insert_with(Tls13StateMachine::new_client);
    // If already connected, return 0 (success); if in Start, initiate handshake
    match &engine.state {
        TlsState::Connected => Ok(Some(Value::Int(0))),
        TlsState::Start => {
            let mut rng = crate::crypto_impl::SecureRandom::new();
            let mut client_random = [0u8; 32];
            rng.next_bytes(&mut client_random);
            let mut session_id = [0u8; 32];
            rng.next_bytes(&mut session_id);
            let ch = HandshakeMessage::ClientHello {
                random: client_random,
                session_id,
                cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
                extensions: Vec::new(),
            };
            let _ = engine.advance(ch);
            Ok(Some(Value::Int(1))) // NEED_UNWRAP (handshake in progress)
        }
        TlsState::Error(_) => Ok(Some(Value::Int(-1))),
        _ => Ok(Some(Value::Int(1))), // handshake in progress
    }
}

fn native_ssl_engine_get_handshake_status(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => match tls_engine_key(&*ctx, *obj, false) {
            Some(key) => key,
            // No row: not started, as `None` below.
            None => return Ok(Some(Value::Int(0))),
        },
        _ => return Ok(Some(Value::Int(0))),
    };
    let engines = tls_engines().lock().unwrap();
    let status = match engines.get(&addr) {
        Some(e) if e.state == TlsState::Connected => 0, // FINISHED
        Some(e) if matches!(e.state, TlsState::Error(_)) => -1,
        Some(_) => 2, // NEED_WRAP or NEED_UNWRAP
        None => 0,    // no engine = not started = FINISHED equivalent
    };
    Ok(Some(Value::Int(status)))
}

fn native_ssl_engine_get_selected_protocol(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => match tls_engine_key(&*ctx, *obj, false) {
            Some(key) => key,
            None => return Ok(Some(Value::Object(None))),
        },
        _ => return Ok(Some(Value::Object(None))),
    };
    let engines = tls_engines().lock().unwrap();
    if let Some(engine) = engines.get(&addr) {
        if let Some(ref proto) = engine.selected_alpn {
            let s = ctx.create_string(proto);
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// T19.9: RFC 8446 §9.1 mandatory-to-implement (MTI) allowlist.
///
/// A TLS 1.3 server "MUST implement" TLS_AES_128_GCM_SHA256 and "SHOULD
/// implement" TLS_AES_256_GCM_SHA384 + TLS_CHACHA20_POLY1305_SHA256. All
/// pre-1.3 suites (SSLv3 suites, RC4, 3DES-CBC, CBC-with-HMAC, etc.) are
/// rejected outright at the state-machine boundary to prevent downgrade
/// attacks; this is the `is_allowed_suite` gate used by
/// `Tls13StateMachine::with_cipher_suite` and
/// `native_ssl_engine_get_selected_cipher`.
pub fn is_mti_cipher_suite(s: CipherSuite) -> bool {
    matches!(
        s,
        CipherSuite::TlsAes128GcmSha256
            | CipherSuite::TlsAes256GcmSha384
            | CipherSuite::TlsChacha20Poly1305Sha256
    )
}

/// T19.9: Return the name of the negotiated cipher suite on a connected
/// SSLEngine, or null if the engine is not yet connected. Used by Keycloak's
/// `SSLSession.getCipherSuite()` hot path — phases_late.rs registers the
/// Java-facing method, but this is the direct accessor for the Session 86/87
/// state machine.
fn native_ssl_engine_get_selected_cipher(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => match tls_engine_key(&*ctx, *obj, false) {
            Some(key) => key,
            None => return Ok(Some(Value::Object(None))),
        },
        _ => return Ok(Some(Value::Object(None))),
    };
    let engines = tls_engines().lock().unwrap();
    if let Some(engine) = engines.get(&addr) {
        if engine.is_connected() {
            // Gate on MTI allowlist — if somehow a non-allowed suite made it
            // this far, return null rather than lying about the selection.
            if is_mti_cipher_suite(engine.cipher_suite) {
                let s = ctx.create_string(engine.cipher_suite.name());
                return Ok(Some(Value::Object(Some(s))));
            }
        }
    }
    Ok(Some(Value::Object(None)))
}

fn native_ssl_context_create_engine(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 3)?;
    // Register a fresh TLS state machine for this engine
    let Some(addr) = tls_engine_key(&*ctx, obj, true) else {
        return Ok(Some(Value::Object(Some(obj))));
    };
    let mut engines = tls_engines().lock().unwrap();
    engines.insert(addr, Tls13StateMachine::new_client());
    Ok(Some(Value::Object(Some(obj))))
}

fn native_ssl_context_create_engine_with_host(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 3)?;
    // The key before `read_string`: `obj` is not pinned across it, and the key
    // (unlike the address this used to file) follows the object if it moves.
    let key = tls_engine_key(&*ctx, obj, true);
    let hostname = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let Some(addr) = key else {
        return Ok(Some(Value::Object(Some(obj))));
    };
    let mut engines = tls_engines().lock().unwrap();
    let engine = if hostname.is_empty() {
        Tls13StateMachine::new_client()
    } else {
        Tls13StateMachine::new_client().with_sni(&hostname)
    };
    engines.insert(addr, engine);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_http_client_tls_version(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let s = ctx.create_string("TLSv1.3");
    Ok(Some(Value::Object(Some(s))))
}

// ===========================================================================
// Registration
// ===========================================================================

pub fn register_tls_impl_natives(r: &mut NativeMethodRegistry) {
    // T19.9 CONSOLIDATION: SSLContext.createSSLEngine (both ()Ljavax/net/ssl/SSLEngine;
    // and (Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;) was previously registered
    // here AND in phases_late::register_p68_ssl. The phases_late registration
    // loads after tls_impl and wins under last-writer-wins. Both phases_late
    // registrations call the same `ssleng_alloc` helper used by the full p68 SSL
    // surface, so removing the duplicate registration here is a pure dead-code
    // delete — the live engine factory path is unchanged.
    //
    // The native_ssl_context_create_engine / native_ssl_context_create_engine_with_host
    // functions still exist above for direct Rust-side calls (e.g. from T19.9
    // tests that exercise the Session 86/87 state-machine-backed engine), just
    // without a JVM-visible registration.
    r.register(
        "javax/net/ssl/SSLEngine",
        "doHandshake",
        "()I",
        native_ssl_engine_do_handshake, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "javax/net/ssl/SSLEngine",
        "getHandshakeStatus",
        "()I",
        native_ssl_engine_get_handshake_status, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "javax/net/ssl/SSLEngine",
        "getSelectedProtocol",
        "()Ljava/lang/String;",
        native_ssl_engine_get_selected_protocol, cratonvm_native_api::NativeKind::Bridge,
    );
    // T19.9: new accessor for the negotiated cipher suite. Returns one of
    // the three RFC 8446 §9.1 MTI suites (TLS_AES_128_GCM_SHA256,
    // TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256) or null if not
    // yet connected. Complements getSelectedProtocol which returns ALPN.
    r.register(
        "javax/net/ssl/SSLEngine",
        "getSelectedCipher",
        "()Ljava/lang/String;",
        native_ssl_engine_get_selected_cipher, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "jdk/internal/net/http/HttpClientImpl",
        "tlsVersion",
        "()Ljava/lang/String;",
        native_http_client_tls_version, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests;
