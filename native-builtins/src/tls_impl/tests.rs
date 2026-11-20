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
