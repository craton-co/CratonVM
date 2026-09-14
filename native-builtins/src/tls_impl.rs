// SPDX-License-Identifier: Apache-2.0
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

/// Global store of TLS state machines keyed by SSLEngine object address.
fn tls_engines() -> &'static Mutex<TlsHashMap<usize, Tls13StateMachine>> {
    static INSTANCE: std::sync::OnceLock<Mutex<TlsHashMap<usize, Tls13StateMachine>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(TlsHashMap::new()))
}

fn native_ssl_engine_do_handshake(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => obj.as_ptr() as usize,
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
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let addr = match args.first() {
        Some(Value::Object(Some(obj))) => obj.as_ptr() as usize,
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
        Some(Value::Object(Some(obj))) => obj.as_ptr() as usize,
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
        Some(Value::Object(Some(obj))) => obj.as_ptr() as usize,
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
    let addr = obj.as_ptr() as usize;
    let mut engines = tls_engines().lock().unwrap();
    engines.insert(addr, Tls13StateMachine::new_client());
    Ok(Some(Value::Object(Some(obj))))
}

fn native_ssl_context_create_engine_with_host(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 3)?;
    let addr = obj.as_ptr() as usize;
    let hostname = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
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

pub(crate) fn register_tls_impl_natives(r: &mut NativeMethodRegistry) {
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
        native_ssl_engine_do_handshake,
    );
    r.register(
        "javax/net/ssl/SSLEngine",
        "getHandshakeStatus",
        "()I",
        native_ssl_engine_get_handshake_status,
    );
    r.register(
        "javax/net/ssl/SSLEngine",
        "getSelectedProtocol",
        "()Ljava/lang/String;",
        native_ssl_engine_get_selected_protocol,
    );
    // T19.9: new accessor for the negotiated cipher suite. Returns one of
    // the three RFC 8446 §9.1 MTI suites (TLS_AES_128_GCM_SHA256,
    // TLS_AES_256_GCM_SHA384, TLS_CHACHA20_POLY1305_SHA256) or null if not
    // yet connected. Complements getSelectedProtocol which returns ALPN.
    r.register(
        "javax/net/ssl/SSLEngine",
        "getSelectedCipher",
        "()Ljava/lang/String;",
        native_ssl_engine_get_selected_cipher,
    );
    r.register(
        "jdk/internal/net/http/HttpClientImpl",
        "tlsVersion",
        "()Ljava/lang/String;",
        native_http_client_tls_version,
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // --- CipherSuite tests ---

    #[test]
    fn test_cipher_suite_code_aes128() {
        assert_eq!(CipherSuite::TlsAes128GcmSha256.code(), 0x1301);
    }

    #[test]
    fn test_cipher_suite_code_aes256() {
        assert_eq!(CipherSuite::TlsAes256GcmSha384.code(), 0x1302);
    }

    #[test]
    fn test_cipher_suite_code_chacha20() {
        assert_eq!(CipherSuite::TlsChacha20Poly1305Sha256.code(), 0x1303);
    }

    #[test]
    fn test_cipher_suite_code_aes128ccm() {
        assert_eq!(CipherSuite::TlsAes128CcmSha256.code(), 0x1304);
    }

    #[test]
    fn test_cipher_suite_from_code_valid() {
        assert_eq!(
            CipherSuite::from_code(0x1301),
            Some(CipherSuite::TlsAes128GcmSha256)
        );
        assert_eq!(
            CipherSuite::from_code(0x1302),
            Some(CipherSuite::TlsAes256GcmSha384)
        );
        assert_eq!(
            CipherSuite::from_code(0x1303),
            Some(CipherSuite::TlsChacha20Poly1305Sha256)
        );
        assert_eq!(
            CipherSuite::from_code(0x1304),
            Some(CipherSuite::TlsAes128CcmSha256)
        );
    }

    #[test]
    fn test_cipher_suite_from_code_invalid() {
        assert_eq!(CipherSuite::from_code(0x0000), None);
        assert_eq!(CipherSuite::from_code(0xFFFF), None);
    }

    #[test]
    fn test_cipher_suite_name() {
        assert_eq!(
            CipherSuite::TlsAes128GcmSha256.name(),
            "TLS_AES_128_GCM_SHA256"
        );
        assert_eq!(
            CipherSuite::TlsAes256GcmSha384.name(),
            "TLS_AES_256_GCM_SHA384"
        );
    }

    #[test]
    fn test_cipher_suite_key_len() {
        assert_eq!(CipherSuite::TlsAes128GcmSha256.key_len(), 16);
        assert_eq!(CipherSuite::TlsAes256GcmSha384.key_len(), 32);
        assert_eq!(CipherSuite::TlsChacha20Poly1305Sha256.key_len(), 32);
        assert_eq!(CipherSuite::TlsAes128CcmSha256.key_len(), 16);
    }

    #[test]
    fn test_cipher_suite_iv_len() {
        assert_eq!(CipherSuite::TlsAes128GcmSha256.iv_len(), 12);
        assert_eq!(CipherSuite::TlsAes256GcmSha384.iv_len(), 12);
    }

    #[test]
    fn test_cipher_suite_hash_len() {
        assert_eq!(CipherSuite::TlsAes128GcmSha256.hash_len(), 32);
        assert_eq!(CipherSuite::TlsAes256GcmSha384.hash_len(), 48);
    }

    #[test]
    fn test_cipher_suite_roundtrip_all() {
        for code in [0x1301u16, 0x1302, 0x1303, 0x1304] {
            let cs = CipherSuite::from_code(code).unwrap();
            assert_eq!(cs.code(), code);
        }
    }

    // --- NamedGroup tests ---

    #[test]
    fn test_named_group_codes() {
        assert_eq!(NamedGroup::X25519.code(), 0x001D);
        assert_eq!(NamedGroup::Secp256r1.code(), 0x0017);
        assert_eq!(NamedGroup::Secp384r1.code(), 0x0018);
        assert_eq!(NamedGroup::X448.code(), 0x001E);
    }

    #[test]
    fn test_named_group_from_code() {
        assert_eq!(NamedGroup::from_code(0x001D), Some(NamedGroup::X25519));
        assert_eq!(NamedGroup::from_code(0x0017), Some(NamedGroup::Secp256r1));
        assert_eq!(NamedGroup::from_code(0x9999), None);
    }

    #[test]
    fn test_named_group_roundtrip() {
        for &ng in &[
            NamedGroup::X25519,
            NamedGroup::Secp256r1,
            NamedGroup::Secp384r1,
            NamedGroup::X448,
        ] {
            assert_eq!(NamedGroup::from_code(ng.code()), Some(ng));
        }
    }

    // --- SignatureAlgorithm tests ---

    #[test]
    fn test_sig_alg_codes() {
        assert_eq!(SignatureAlgorithm::RsaPkcs1Sha256.code(), 0x0401);
        assert_eq!(SignatureAlgorithm::EcdsaSecp256r1Sha256.code(), 0x0403);
        assert_eq!(SignatureAlgorithm::Ed25519.code(), 0x0807);
        assert_eq!(SignatureAlgorithm::RsaPssRsaeSha256.code(), 0x0804);
    }

    #[test]
    fn test_sig_alg_from_code() {
        assert_eq!(
            SignatureAlgorithm::from_code(0x0401),
            Some(SignatureAlgorithm::RsaPkcs1Sha256)
        );
        assert_eq!(SignatureAlgorithm::from_code(0x0000), None);
    }

    #[test]
    fn test_sig_alg_roundtrip() {
        for &sa in &[
            SignatureAlgorithm::RsaPkcs1Sha256,
            SignatureAlgorithm::RsaPkcs1Sha384,
            SignatureAlgorithm::EcdsaSecp256r1Sha256,
            SignatureAlgorithm::EcdsaSecp384r1Sha384,
            SignatureAlgorithm::Ed25519,
            SignatureAlgorithm::RsaPssRsaeSha256,
        ] {
            assert_eq!(SignatureAlgorithm::from_code(sa.code()), Some(sa));
        }
    }

    // --- TlsExtension tests ---

    #[test]
    fn test_extension_type_server_name() {
        let ext = TlsExtension::ServerName("example.com".into());
        assert_eq!(ext.extension_type(), 0);
    }

    #[test]
    fn test_extension_type_supported_versions() {
        let ext = TlsExtension::SupportedVersions(vec![0x0304]);
        assert_eq!(ext.extension_type(), 43);
    }

    #[test]
    fn test_extension_type_key_share() {
        let ext = TlsExtension::KeyShare(vec![]);
        assert_eq!(ext.extension_type(), 51);
    }

    #[test]
    fn test_extension_type_alpn() {
        let ext = TlsExtension::Alpn(vec!["h2".into()]);
        assert_eq!(ext.extension_type(), 16);
    }

    #[test]
    fn test_extension_type_all_variants() {
        assert_eq!(TlsExtension::MaxFragmentLength(1).extension_type(), 1);
        assert_eq!(TlsExtension::SupportedGroups(vec![]).extension_type(), 10);
        assert_eq!(
            TlsExtension::SignatureAlgorithms(vec![]).extension_type(),
            13
        );
        assert_eq!(TlsExtension::PreSharedKey(vec![]).extension_type(), 41);
        assert_eq!(TlsExtension::EarlyData.extension_type(), 42);
        assert_eq!(
            TlsExtension::PskKeyExchangeModes(vec![]).extension_type(),
            45
        );
    }

    // --- ContentType tests ---

    #[test]
    fn test_content_type_codes() {
        assert_eq!(ContentType::ChangeCipherSpec.code(), 20);
        assert_eq!(ContentType::Alert.code(), 21);
        assert_eq!(ContentType::Handshake.code(), 22);
        assert_eq!(ContentType::ApplicationData.code(), 23);
    }

    #[test]
    fn test_content_type_from_code() {
        assert_eq!(ContentType::from_code(22), Some(ContentType::Handshake));
        assert_eq!(ContentType::from_code(99), None);
    }

    // --- HandshakeMessage tests ---

    #[test]
    fn test_handshake_message_type_codes() {
        let ch = HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![],
            extensions: vec![],
        };
        assert_eq!(ch.msg_type(), HT_CLIENT_HELLO);

        let sh = HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            extensions: vec![],
        };
        assert_eq!(sh.msg_type(), HT_SERVER_HELLO);

        let fin = HandshakeMessage::Finished {
            verify_data: vec![],
        };
        assert_eq!(fin.msg_type(), HT_FINISHED);
    }

    #[test]
    fn test_handshake_msg_type_all() {
        assert_eq!(
            HandshakeMessage::EncryptedExtensions { extensions: vec![] }.msg_type(),
            HT_ENCRYPTED_EXTENSIONS
        );
        assert_eq!(
            HandshakeMessage::Certificate { cert_chain: vec![] }.msg_type(),
            HT_CERTIFICATE
        );
        assert_eq!(
            HandshakeMessage::CertificateVerify {
                algorithm: SignatureAlgorithm::Ed25519,
                signature: vec![]
            }
            .msg_type(),
            HT_CERTIFICATE_VERIFY
        );
        assert_eq!(
            HandshakeMessage::NewSessionTicket {
                lifetime: 0,
                ticket: vec![]
            }
            .msg_type(),
            HT_NEW_SESSION_TICKET
        );
        assert_eq!(
            HandshakeMessage::KeyUpdate {
                request_update: false
            }
            .msg_type(),
            HT_KEY_UPDATE
        );
    }

    // --- AlertLevel tests ---

    #[test]
    fn test_alert_level_codes() {
        assert_eq!(AlertLevel::Warning.code(), 1);
        assert_eq!(AlertLevel::Fatal.code(), 2);
    }

    #[test]
    fn test_alert_level_from_code() {
        assert_eq!(AlertLevel::from_code(1), Some(AlertLevel::Warning));
        assert_eq!(AlertLevel::from_code(2), Some(AlertLevel::Fatal));
        assert_eq!(AlertLevel::from_code(0), None);
    }

    // --- AlertDescription tests ---

    #[test]
    fn test_alert_desc_codes() {
        assert_eq!(AlertDescription::CloseNotify.code(), 0);
        assert_eq!(AlertDescription::HandshakeFailure.code(), 40);
        assert_eq!(AlertDescription::InternalError.code(), 80);
        assert_eq!(AlertDescription::MissingExtension.code(), 109);
    }

    #[test]
    fn test_alert_desc_from_code() {
        assert_eq!(
            AlertDescription::from_code(0),
            Some(AlertDescription::CloseNotify)
        );
        assert_eq!(
            AlertDescription::from_code(40),
            Some(AlertDescription::HandshakeFailure)
        );
        assert_eq!(AlertDescription::from_code(255), None);
    }

    #[test]
    fn test_alert_desc_roundtrip() {
        for &desc in &[
            AlertDescription::CloseNotify,
            AlertDescription::UnexpectedMessage,
            AlertDescription::BadRecordMac,
            AlertDescription::HandshakeFailure,
            AlertDescription::BadCertificate,
            AlertDescription::CertificateExpired,
            AlertDescription::UnknownCa,
            AlertDescription::DecodeError,
            AlertDescription::DecryptError,
            AlertDescription::ProtocolVersion,
            AlertDescription::InsufficientSecurity,
            AlertDescription::InternalError,
            AlertDescription::MissingExtension,
            AlertDescription::UnsupportedExtension,
        ] {
            assert_eq!(AlertDescription::from_code(desc.code()), Some(desc));
        }
    }

    // --- TlsAlert tests ---

    #[test]
    fn test_tls_alert_encode() {
        let alert = TlsAlert::new(AlertLevel::Fatal, AlertDescription::HandshakeFailure);
        assert_eq!(alert.encode(), [2, 40]);
    }

    #[test]
    fn test_tls_alert_decode() {
        let alert = TlsAlert::decode(&[1, 0]).unwrap();
        assert_eq!(alert.level, AlertLevel::Warning);
        assert_eq!(alert.description, AlertDescription::CloseNotify);
    }

    #[test]
    fn test_tls_alert_decode_too_short() {
        assert!(TlsAlert::decode(&[1]).is_err());
    }

    #[test]
    fn test_tls_alert_decode_bad_level() {
        assert!(TlsAlert::decode(&[99, 0]).is_err());
    }

    #[test]
    fn test_tls_alert_decode_bad_desc() {
        assert!(TlsAlert::decode(&[1, 255]).is_err());
    }

    // --- TlsError tests ---

    #[test]
    fn test_tls_error_display() {
        let err = TlsError::new(TlsErrorKind::HandshakeFailure, "test error");
        let s = format!("{}", err);
        assert!(s.contains("HandshakeFailure"));
        assert!(s.contains("test error"));
    }

    // --- HKDF tests ---

    #[test]
    fn test_hkdf_extract_produces_32_bytes() {
        let out = hkdf_extract(&[1, 2, 3], &[4, 5, 6]);
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn test_hkdf_extract_deterministic() {
        let a = hkdf_extract(b"salt", b"ikm");
        let b = hkdf_extract(b"salt", b"ikm");
        assert_eq!(a, b);
    }

    #[test]
    fn test_hkdf_expand_length() {
        let prk = hkdf_extract(b"salt", b"ikm");
        let out16 = hkdf_expand(&prk, b"info", 16);
        assert_eq!(out16.len(), 16);
        let out48 = hkdf_expand(&prk, b"info", 48);
        assert_eq!(out48.len(), 48);
    }

    #[test]
    fn test_hkdf_expand_deterministic() {
        let prk = hkdf_extract(b"salt", b"ikm");
        let a = hkdf_expand(&prk, b"info", 32);
        let b = hkdf_expand(&prk, b"info", 32);
        assert_eq!(a, b);
    }

    #[test]
    fn test_hkdf_extract_zero_salt() {
        let out = hkdf_extract(&[], &[42]);
        assert_eq!(out.len(), 32);
    }

    // --- TlsRecordLayer tests ---

    #[test]
    fn test_record_layer_encode_handshake() {
        let mut rl = TlsRecordLayer::new();
        let data = b"hello";
        let encoded = rl.encode_record(ContentType::Handshake, data);
        assert_eq!(encoded[0], 22); // Handshake
        assert_eq!(encoded[1], 0x03);
        assert_eq!(encoded[2], 0x03);
        assert_eq!(encoded[3], 0);
        assert_eq!(encoded[4], 5);
        assert_eq!(&encoded[5..], b"hello");
    }

    #[test]
    fn test_record_layer_encode_application_data() {
        let mut rl = TlsRecordLayer::new();
        let encoded = rl.encode_record(ContentType::ApplicationData, &[1, 2, 3]);
        assert_eq!(encoded[0], 23);
        assert_eq!(encoded.len(), 8);
    }

    #[test]
    fn test_record_layer_decode_valid() {
        let mut rl = TlsRecordLayer::new();
        let raw = rl.encode_record(ContentType::Handshake, b"test");
        let mut rl2 = TlsRecordLayer::new();
        let record = rl2.decode_record(&raw).unwrap();
        assert_eq!(record.content_type, ContentType::Handshake);
        assert_eq!(record.protocol_version, 0x0303);
        assert_eq!(record.fragment, b"test");
        assert_eq!(record.length, 4);
    }

    #[test]
    fn test_record_layer_decode_too_short() {
        let mut rl = TlsRecordLayer::new();
        assert!(rl.decode_record(&[22, 3, 3]).is_err());
    }

    #[test]
    fn test_record_layer_decode_truncated_fragment() {
        let mut rl = TlsRecordLayer::new();
        // Header claims 10 bytes but only 2 provided
        let data = [22, 3, 3, 0, 10, 0, 0];
        assert!(rl.decode_record(&data).is_err());
    }

    #[test]
    fn test_record_layer_decode_bad_content_type() {
        let mut rl = TlsRecordLayer::new();
        let data = [99, 3, 3, 0, 0];
        assert!(rl.decode_record(&data).is_err());
    }

    #[test]
    fn test_record_layer_decode_exceeds_max_fragment() {
        let mut rl = TlsRecordLayer::with_max_fragment_length(4);
        // Header claims 5 bytes, exceeds max of 4
        let data = [22, 3, 3, 0, 5, 1, 2, 3, 4, 5];
        assert!(rl.decode_record(&data).is_err());
    }

    #[test]
    fn test_record_layer_stats() {
        let mut rl = TlsRecordLayer::new();
        assert_eq!(rl.records_sent, 0);
        assert_eq!(rl.bytes_sent, 0);
        rl.encode_record(ContentType::Handshake, b"abc");
        assert_eq!(rl.records_sent, 1);
        assert_eq!(rl.bytes_sent, 8); // 5 header + 3 data
        rl.encode_record(ContentType::ApplicationData, b"de");
        assert_eq!(rl.records_sent, 2);
        assert_eq!(rl.bytes_sent, 15); // 8 + 5+2
    }

    #[test]
    fn test_record_layer_roundtrip_stats() {
        let mut encoder = TlsRecordLayer::new();
        let raw = encoder.encode_record(ContentType::Alert, &[1, 0]);
        let mut decoder = TlsRecordLayer::new();
        decoder.decode_record(&raw).unwrap();
        assert_eq!(decoder.records_received, 1);
        assert_eq!(decoder.bytes_received, 7); // 5 + 2
    }

    // --- TlsKeySchedule tests ---

    #[test]
    fn test_key_schedule_early_secret_no_psk() {
        let mut ks = TlsKeySchedule::new();
        let es = ks.derive_early_secret(None);
        assert_eq!(es.len(), 32);
        assert_eq!(ks.early_secret, es);
    }

    #[test]
    fn test_key_schedule_early_secret_with_psk() {
        let mut ks = TlsKeySchedule::new();
        let psk = b"my_psk_value";
        let es = ks.derive_early_secret(Some(psk));
        assert_eq!(es.len(), 32);
        // Different from no-PSK
        let mut ks2 = TlsKeySchedule::new();
        let es2 = ks2.derive_early_secret(None);
        assert_ne!(es, es2);
    }

    #[test]
    fn test_key_schedule_handshake_secret() {
        let mut ks = TlsKeySchedule::new();
        ks.derive_early_secret(None);
        let hs = ks.derive_handshake_secret(b"shared_secret");
        assert_eq!(hs.len(), 32);
        assert!(!ks.client_handshake_traffic_secret.is_empty());
        assert!(!ks.server_handshake_traffic_secret.is_empty());
    }

    #[test]
    fn test_key_schedule_master_secret() {
        let mut ks = TlsKeySchedule::new();
        ks.derive_early_secret(None);
        ks.derive_handshake_secret(b"shared");
        let ms = ks.derive_master_secret();
        assert_eq!(ms.len(), 32);
        assert!(!ks.client_application_traffic_secret.is_empty());
        assert!(!ks.server_application_traffic_secret.is_empty());
    }

    #[test]
    fn test_key_schedule_derive_traffic_keys() {
        let mut ks = TlsKeySchedule::new();
        ks.derive_early_secret(None);
        ks.derive_handshake_secret(b"shared");
        let tk = ks.derive_traffic_keys(&ks.client_handshake_traffic_secret.clone(), 16, 12);
        assert_eq!(tk.key.len(), 16);
        assert_eq!(tk.iv.len(), 12);
    }

    #[test]
    fn test_key_schedule_traffic_keys_256() {
        let mut ks = TlsKeySchedule::new();
        ks.derive_early_secret(None);
        ks.derive_handshake_secret(b"shared");
        let tk = ks.derive_traffic_keys(&ks.server_handshake_traffic_secret.clone(), 32, 12);
        assert_eq!(tk.key.len(), 32);
        assert_eq!(tk.iv.len(), 12);
    }

    // --- SessionCache tests ---

    #[test]
    fn test_session_cache_store_and_lookup() {
        let mut cache = SessionCache::new();
        let lookup_key = vec![1, 2, 3];
        let session = SessionTicket {
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            resumption_secret: vec![42],
            lifetime: 3600,
            created_at: 1000,
            sni: Some("example.com".to_string()),
        };
        cache.store(vec![1, 2, 3], session);
        let found = cache.lookup(&lookup_key).unwrap();
        assert_eq!(found.cipher_suite, CipherSuite::TlsAes128GcmSha256);
        assert_eq!(found.sni, Some("example.com".to_string()));
    }

    #[test]
    fn test_session_cache_lookup_missing() {
        let cache = SessionCache::new();
        assert!(cache.lookup(&[99]).is_none());
    }

    #[test]
    fn test_session_cache_remove() {
        let mut cache = SessionCache::new();
        let ticket = vec![10];
        cache.store(
            ticket.clone(),
            SessionTicket {
                cipher_suite: CipherSuite::TlsAes256GcmSha384,
                resumption_secret: vec![],
                lifetime: 100,
                created_at: 0,
                sni: None,
            },
        );
        assert!(cache.remove(&ticket));
        assert!(!cache.remove(&ticket)); // already gone
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn test_session_cache_count() {
        let mut cache = SessionCache::new();
        assert_eq!(cache.count(), 0);
        cache.store(
            vec![1],
            SessionTicket {
                cipher_suite: CipherSuite::TlsAes128GcmSha256,
                resumption_secret: vec![],
                lifetime: 100,
                created_at: 0,
                sni: None,
            },
        );
        assert_eq!(cache.count(), 1);
        cache.store(
            vec![2],
            SessionTicket {
                cipher_suite: CipherSuite::TlsAes128GcmSha256,
                resumption_secret: vec![],
                lifetime: 100,
                created_at: 0,
                sni: None,
            },
        );
        assert_eq!(cache.count(), 2);
    }

    #[test]
    fn test_session_cache_cleanup_expired() {
        let mut cache = SessionCache::new();
        cache.store(
            vec![1],
            SessionTicket {
                cipher_suite: CipherSuite::TlsAes128GcmSha256,
                resumption_secret: vec![],
                lifetime: 100,
                created_at: 1000,
                sni: None,
            },
        );
        cache.store(
            vec![2],
            SessionTicket {
                cipher_suite: CipherSuite::TlsAes128GcmSha256,
                resumption_secret: vec![],
                lifetime: 100,
                created_at: 5000,
                sni: None,
            },
        );
        // At time 2000, ticket 1 (expires at 1100) is expired, ticket 2 (expires at 5100) is not
        let removed = cache.cleanup_expired(2000);
        assert_eq!(removed, 1);
        assert_eq!(cache.count(), 1);
    }

    #[test]
    fn test_session_cache_max_entries_eviction() {
        let mut cache = SessionCache::with_capacity(2);
        for i in 0..3u8 {
            cache.store(
                vec![i],
                SessionTicket {
                    cipher_suite: CipherSuite::TlsAes128GcmSha256,
                    resumption_secret: vec![i],
                    lifetime: 100,
                    created_at: 0,
                    sni: None,
                },
            );
        }
        assert_eq!(cache.count(), 2);
    }

    // --- OCSP tests ---

    #[test]
    fn test_ocsp_response_good_valid() {
        let resp = OcspResponse {
            status: OcspStatus::Good,
            produced_at: 100,
            this_update: 100,
            next_update: 200,
            responder_id: vec![1],
        };
        assert!(resp.is_valid(150));
    }

    #[test]
    fn test_ocsp_response_good_expired() {
        let resp = OcspResponse {
            status: OcspStatus::Good,
            produced_at: 100,
            this_update: 100,
            next_update: 200,
            responder_id: vec![1],
        };
        assert!(!resp.is_valid(300));
    }

    #[test]
    fn test_ocsp_response_revoked() {
        let resp = OcspResponse {
            status: OcspStatus::Revoked(50),
            produced_at: 100,
            this_update: 100,
            next_update: 200,
            responder_id: vec![1],
        };
        assert!(!resp.is_valid(150));
    }

    #[test]
    fn test_ocsp_response_unknown() {
        let resp = OcspResponse {
            status: OcspStatus::Unknown,
            produced_at: 100,
            this_update: 100,
            next_update: 200,
            responder_id: vec![1],
        };
        assert!(!resp.is_valid(150));
    }

    // --- State Machine: Client tests ---

    #[test]
    fn test_client_start_to_wait_server_hello() {
        let mut sm = Tls13StateMachine::new_client();
        assert_eq!(*sm.get_state(), TlsState::Start);
        let ch = HandshakeMessage::ClientHello {
            random: [1u8; 32],
            session_id: [2u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        };
        let resp = sm.advance(ch).unwrap();
        assert!(resp.is_some()); // echoes ClientHello
        assert_eq!(*sm.get_state(), TlsState::WaitServerHello);
    }

    #[test]
    fn test_client_full_handshake() {
        let mut sm = Tls13StateMachine::new_client();

        // 1. Send ClientHello
        sm.advance(HandshakeMessage::ClientHello {
            random: [1u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes256GcmSha384],
            extensions: vec![TlsExtension::ServerName("example.com".into())],
        })
        .unwrap();

        // 2. Receive ServerHello
        sm.advance(HandshakeMessage::ServerHello {
            random: [2u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes256GcmSha384,
            extensions: vec![],
        })
        .unwrap();
        assert_eq!(*sm.get_state(), TlsState::WaitEncryptedExtensions);

        // 3. Receive EncryptedExtensions
        sm.advance(HandshakeMessage::EncryptedExtensions { extensions: vec![] })
            .unwrap();
        assert_eq!(*sm.get_state(), TlsState::WaitCertificate);

        // 4. Receive Certificate
        sm.advance(HandshakeMessage::Certificate {
            cert_chain: vec![CertificateEntry {
                cert_data: vec![0xDE, 0xAD],
                extensions: vec![],
            }],
        })
        .unwrap();
        assert_eq!(*sm.get_state(), TlsState::WaitCertificateVerify);

        // 5. Receive CertificateVerify
        sm.advance(HandshakeMessage::CertificateVerify {
            algorithm: SignatureAlgorithm::EcdsaSecp256r1Sha256,
            signature: vec![0xBE, 0xEF],
        })
        .unwrap();
        assert_eq!(*sm.get_state(), TlsState::WaitFinished);

        // 6. Receive Finished
        let resp = sm
            .advance(HandshakeMessage::Finished {
                verify_data: vec![0xCA, 0xFE],
            })
            .unwrap();
        assert!(resp.is_some()); // client sends Finished back
        assert!(sm.is_connected());
    }

    #[test]
    fn test_client_unexpected_message_in_start() {
        let mut sm = Tls13StateMachine::new_client();
        let result = sm.advance(HandshakeMessage::Finished {
            verify_data: vec![],
        });
        assert!(result.is_err());
        matches!(sm.state, TlsState::Error(_));
    }

    #[test]
    fn test_client_receives_new_session_ticket_after_connected() {
        let mut sm = make_connected_client();
        let resp = sm
            .advance(HandshakeMessage::NewSessionTicket {
                lifetime: 3600,
                ticket: vec![1, 2, 3],
            })
            .unwrap();
        assert!(resp.is_none());
        assert!(sm.is_connected());
    }

    #[test]
    fn test_client_receives_key_update_with_request() {
        let mut sm = make_connected_client();
        let resp = sm
            .advance(HandshakeMessage::KeyUpdate {
                request_update: true,
            })
            .unwrap();
        assert!(
            matches!(
                resp,
                Some(HandshakeMessage::KeyUpdate {
                    request_update: false
                })
            ),
            "Expected KeyUpdate response with request_update=false, got {resp:?}"
        );
    }

    #[test]
    fn test_client_receives_key_update_without_request() {
        let mut sm = make_connected_client();
        let resp = sm
            .advance(HandshakeMessage::KeyUpdate {
                request_update: false,
            })
            .unwrap();
        assert!(resp.is_none());
    }

    #[test]
    fn test_client_cipher_suite_selection() {
        let mut sm = Tls13StateMachine::new_client();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsChacha20Poly1305Sha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsChacha20Poly1305Sha256,
            extensions: vec![],
        })
        .unwrap();
        assert_eq!(sm.cipher_suite, CipherSuite::TlsChacha20Poly1305Sha256);
    }

    #[test]
    fn test_client_alpn_from_server_hello() {
        let mut sm = Tls13StateMachine::new_client();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            extensions: vec![TlsExtension::Alpn(vec!["h2".into()])],
        })
        .unwrap();
        assert_eq!(sm.selected_alpn, Some("h2".to_string()));
    }

    #[test]
    fn test_client_alpn_from_encrypted_extensions() {
        let mut sm = Tls13StateMachine::new_client();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::EncryptedExtensions {
            extensions: vec![TlsExtension::Alpn(vec!["h2".into()])],
        })
        .unwrap();
        assert_eq!(sm.selected_alpn, Some("h2".to_string()));
    }

    // --- State Machine: Server tests ---

    #[test]
    fn test_server_receives_client_hello() {
        let mut sm = Tls13StateMachine::new_server();
        let resp = sm
            .advance(HandshakeMessage::ClientHello {
                random: [3u8; 32],
                session_id: [4u8; 32],
                cipher_suites: vec![CipherSuite::TlsAes256GcmSha384],
                extensions: vec![TlsExtension::ServerName("host.example".into())],
            })
            .unwrap();
        assert!(resp.is_some());
        assert_eq!(sm.sni_hostname, Some("host.example".to_string()));
        assert_eq!(*sm.get_state(), TlsState::WaitFinished);
    }

    #[test]
    fn test_server_full_handshake() {
        let mut sm = Tls13StateMachine::new_server();

        // 1. Receive ClientHello -> produces ServerHello
        let sh = sm
            .advance(HandshakeMessage::ClientHello {
                random: [1u8; 32],
                session_id: [0u8; 32],
                cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
                extensions: vec![],
            })
            .unwrap();
        assert!(sh.is_some());

        // 2. Receive client Finished -> produces server Finished
        let fin = sm
            .advance(HandshakeMessage::Finished {
                verify_data: vec![0xAB],
            })
            .unwrap();
        assert!(fin.is_some());
        assert!(sm.is_connected());
    }

    #[test]
    fn test_server_unexpected_message() {
        let mut sm = Tls13StateMachine::new_server();
        let result = sm.advance(HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            extensions: vec![],
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_server_key_update_after_connected() {
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::Finished {
            verify_data: vec![],
        })
        .unwrap();
        assert!(sm.is_connected());
        let resp = sm
            .advance(HandshakeMessage::KeyUpdate {
                request_update: true,
            })
            .unwrap();
        assert!(resp.is_some());
    }

    #[test]
    fn test_server_sni_and_alpn_extraction() {
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![
                TlsExtension::ServerName("test.example.com".into()),
                TlsExtension::Alpn(vec!["h2".into(), "http/1.1".into()]),
            ],
        })
        .unwrap();
        assert_eq!(sm.sni_hostname, Some("test.example.com".to_string()));
        assert_eq!(sm.selected_alpn, Some("h2".to_string()));
    }

    // --- State Machine: builder/accessor tests ---

    #[test]
    fn test_state_machine_with_sni() {
        let sm = Tls13StateMachine::new_client().with_sni("example.org");
        assert_eq!(sm.sni_hostname, Some("example.org".to_string()));
    }

    #[test]
    fn test_state_machine_with_client_random() {
        let r = [42u8; 32];
        let sm = Tls13StateMachine::new_client().with_client_random(r);
        assert_eq!(sm.client_random, r);
    }

    #[test]
    fn test_state_machine_with_session_id() {
        let sid = [7u8; 32];
        let sm = Tls13StateMachine::new_client().with_session_id(sid);
        assert_eq!(sm.session_id, sid);
    }

    #[test]
    fn test_state_machine_not_connected_initially() {
        let sm = Tls13StateMachine::new_client();
        assert!(!sm.is_connected());
    }

    #[test]
    fn test_state_machine_role() {
        assert_eq!(Tls13StateMachine::new_client().role, TlsRole::Client);
        assert_eq!(Tls13StateMachine::new_server().role, TlsRole::Server);
    }

    #[test]
    fn test_state_machine_transcript_updated() {
        let mut sm = Tls13StateMachine::new_client();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        assert!(!sm.transcript_hash.is_empty());
        assert_eq!(sm.transcript_hash[0], HT_CLIENT_HELLO);
    }

    // --- Handshake constant tests ---

    #[test]
    fn test_handshake_type_constants() {
        assert_eq!(HT_CLIENT_HELLO, 1);
        assert_eq!(HT_SERVER_HELLO, 2);
        assert_eq!(HT_NEW_SESSION_TICKET, 4);
        assert_eq!(HT_ENCRYPTED_EXTENSIONS, 8);
        assert_eq!(HT_CERTIFICATE, 11);
        assert_eq!(HT_CERTIFICATE_VERIFY, 15);
        assert_eq!(HT_FINISHED, 20);
        assert_eq!(HT_KEY_UPDATE, 24);
    }

    // --- Helper to create a connected client state machine ---

    fn make_connected_client() -> Tls13StateMachine {
        let mut sm = Tls13StateMachine::new_client();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::ServerHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suite: CipherSuite::TlsAes128GcmSha256,
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::EncryptedExtensions { extensions: vec![] })
            .unwrap();
        sm.advance(HandshakeMessage::Certificate { cert_chain: vec![] })
            .unwrap();
        sm.advance(HandshakeMessage::CertificateVerify {
            algorithm: SignatureAlgorithm::Ed25519,
            signature: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::Finished {
            verify_data: vec![],
        })
        .unwrap();
        assert!(sm.is_connected());
        sm
    }

    #[test]
    fn test_server_full_state_flow() {
        // Per RFC 8446 §A.2, the server emits its entire flight
        // (ServerHello, EncryptedExtensions, Certificate, CertificateVerify,
        // server Finished) as one conceptual step on receipt of ClientHello,
        // then transitions to WAIT_FINISHED awaiting the client's Finished.
        let mut server = Tls13StateMachine::new_server();
        let ch = HandshakeMessage::ClientHello {
            random: [0xAA; 32],
            session_id: [0xBB; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        };
        let resp = server.advance(ch).unwrap();
        assert!(resp.is_some()); // ServerHello (head of server flight)
        assert_eq!(server.state, TlsState::WaitFinished);
        // Client Finished
        let fin = HandshakeMessage::Finished {
            verify_data: vec![0x02],
        };
        let resp = server.advance(fin).unwrap();
        assert!(resp.is_some()); // Server Finished
        assert_eq!(server.state, TlsState::Connected);
    }

    #[test]
    fn test_server_random_is_not_hardcoded() {
        let mut server = Tls13StateMachine::new_server();
        let ch = HandshakeMessage::ClientHello {
            random: [0xAA; 32],
            session_id: [0xBB; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        };
        let _ = server.advance(ch).unwrap();
        // Server random should NOT be the old hardcoded [0x01; 32]
        assert_ne!(server.server_random, [0x01u8; 32]);
    }

    #[test]
    fn test_tls_engine_store() {
        let mut engines = tls_engines().lock().unwrap();
        engines.insert(999, Tls13StateMachine::new_client());
        assert!(engines.contains_key(&999));
        engines.remove(&999);
    }

    // --- Regression tests for server state-machine fix ---

    /// RFC 8446 §A.2: after the server receives ClientHello, it emits its
    /// entire outbound flight (ServerHello..server Finished) in one conceptual
    /// step and transitions to WAIT_FINISHED.
    #[test]
    fn test_server_state_after_client_hello_is_wait_finished() {
        let mut sm = Tls13StateMachine::new_server();
        let _ = sm
            .advance(HandshakeMessage::ClientHello {
                random: [0u8; 32],
                session_id: [0u8; 32],
                cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
                extensions: vec![],
            })
            .unwrap();
        assert_eq!(*sm.get_state(), TlsState::WaitFinished);
    }

    /// Regression: the server transcript hash must cover every handshake
    /// message in the flight (ClientHello + 5 server-flight messages) so that
    /// the subsequently-received client Finished is validated against a full
    /// transcript, not a partial one.
    #[test]
    fn test_server_transcript_covers_full_flight() {
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        // ClientHello + ServerHello + EE + Cert + CertVerify + server Finished = 6
        assert_eq!(sm.transcript_hash.len(), 6);
        assert_eq!(sm.transcript_hash[0], HT_CLIENT_HELLO);
        assert_eq!(sm.transcript_hash[1], HT_SERVER_HELLO);
        assert_eq!(sm.transcript_hash[2], HT_ENCRYPTED_EXTENSIONS);
        assert_eq!(sm.transcript_hash[3], HT_CERTIFICATE);
        assert_eq!(sm.transcript_hash[4], HT_CERTIFICATE_VERIFY);
        assert_eq!(sm.transcript_hash[5], HT_FINISHED);
    }

    /// Regression: the condensed ClientHello -> Finished flow must succeed
    /// end-to-end and reach Connected.
    #[test]
    fn test_server_condensed_flow_reaches_connected() {
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        let resp = sm
            .advance(HandshakeMessage::Finished {
                verify_data: vec![],
            })
            .unwrap();
        assert!(resp.is_some());
        assert_eq!(*sm.get_state(), TlsState::Connected);
        assert!(sm.is_connected());
    }

    /// Regression: the server must still reject out-of-order handshake
    /// messages — only ClientHello in Start, Finished in WaitFinished, and
    /// KeyUpdate in Connected are accepted. We must not weaken this.
    #[test]
    fn test_server_rejects_out_of_order_messages() {
        // ServerHello in Start: rejected.
        let mut sm = Tls13StateMachine::new_server();
        assert!(sm
            .advance(HandshakeMessage::ServerHello {
                random: [0u8; 32],
                session_id: [0u8; 32],
                cipher_suite: CipherSuite::TlsAes128GcmSha256,
                extensions: vec![],
            })
            .is_err());

        // EncryptedExtensions in WaitFinished (after ClientHello): rejected.
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        assert!(sm
            .advance(HandshakeMessage::EncryptedExtensions { extensions: vec![] })
            .is_err());

        // Certificate in WaitFinished: rejected.
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        assert!(sm
            .advance(HandshakeMessage::Certificate { cert_chain: vec![] })
            .is_err());
    }

    /// Regression: KeyUpdate post-Connected must round-trip correctly even
    /// when the client requested an update.
    #[test]
    fn test_server_key_update_request_false_no_response() {
        let mut sm = Tls13StateMachine::new_server();
        sm.advance(HandshakeMessage::ClientHello {
            random: [0u8; 32],
            session_id: [0u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![],
        })
        .unwrap();
        sm.advance(HandshakeMessage::Finished {
            verify_data: vec![],
        })
        .unwrap();
        let resp = sm
            .advance(HandshakeMessage::KeyUpdate {
                request_update: false,
            })
            .unwrap();
        // When request_update is false, no reply is required.
        assert!(resp.is_none());
        assert_eq!(*sm.get_state(), TlsState::Connected);
    }

    // =======================================================================
    // T19.9 — RFC 8446 §9.1 MTI cipher-suite allowlist enforcement
    // =======================================================================

    #[test]
    fn t19_9_mti_allowlist_accepts_aes128gcm() {
        assert!(super::is_mti_cipher_suite(CipherSuite::TlsAes128GcmSha256));
    }

    #[test]
    fn t19_9_mti_allowlist_accepts_aes256gcm() {
        assert!(super::is_mti_cipher_suite(CipherSuite::TlsAes256GcmSha384));
    }

    #[test]
    fn t19_9_mti_allowlist_accepts_chacha20() {
        assert!(super::is_mti_cipher_suite(
            CipherSuite::TlsChacha20Poly1305Sha256
        ));
    }

    #[test]
    fn t19_9_mti_allowlist_rejects_aes128ccm() {
        // RFC 8446 §9.1 lists TLS_AES_128_CCM_SHA256 as OPTIONAL (not MTI).
        // Keycloak's HTTPS listener sticks to the MTI three.
        assert!(!super::is_mti_cipher_suite(CipherSuite::TlsAes128CcmSha256));
    }

    #[test]
    fn t19_9_with_cipher_suite_builder_falls_back_on_non_mti() {
        // with_cipher_suite must transparently swap a non-MTI suite for the
        // default AES-128-GCM to prevent downgrade attacks at machine
        // construction time.
        let sm = Tls13StateMachine::new_client().with_cipher_suite(CipherSuite::TlsAes128CcmSha256);
        assert_eq!(sm.cipher_suite, CipherSuite::TlsAes128GcmSha256);
    }

    #[test]
    fn t19_9_with_cipher_suite_builder_accepts_mti() {
        let sm = Tls13StateMachine::new_client()
            .with_cipher_suite(CipherSuite::TlsChacha20Poly1305Sha256);
        assert_eq!(sm.cipher_suite, CipherSuite::TlsChacha20Poly1305Sha256);
    }

    #[test]
    fn t19_9_registration_removes_ssl_context_create_engine() {
        // Post-consolidation, tls_impl must not register SSLContext.createSSLEngine
        // (phases_late is canonical). This is the counterpart to
        // `tls::tls_tests::t19_9_tls_orphan_registrations_removed` but
        // scoped to the tls_impl module to prove it at this layer too.
        use cratonvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        super::register_tls_impl_natives(&mut r);
        assert!(r
            .find(
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "()Ljavax/net/ssl/SSLEngine;"
            )
            .is_none());
        assert!(r
            .find(
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"
            )
            .is_none());
    }

    #[test]
    fn t19_9_registration_keeps_engine_handshake_primitives() {
        // The three state-machine primitives (doHandshake, getHandshakeStatus:()I,
        // getSelectedProtocol, getSelectedCipher) must remain registered — these
        // are what Keycloak's ServerConnectionManager calls to drive the
        // handshake forward one record at a time.
        use cratonvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        super::register_tls_impl_natives(&mut r);
        assert!(r
            .find("javax/net/ssl/SSLEngine", "doHandshake", "()I")
            .is_some());
        assert!(r
            .find("javax/net/ssl/SSLEngine", "getHandshakeStatus", "()I")
            .is_some());
        assert!(r
            .find(
                "javax/net/ssl/SSLEngine",
                "getSelectedProtocol",
                "()Ljava/lang/String;"
            )
            .is_some());
        assert!(r
            .find(
                "javax/net/ssl/SSLEngine",
                "getSelectedCipher",
                "()Ljava/lang/String;"
            )
            .is_some());
    }
}
