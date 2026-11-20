use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn compute_mac_is_deterministic_and_key_sensitive() {
    let key_a = [0x0bu8; 20];
    let key_b = [0x0cu8; 20];
    let data = b"Hi There";
    let header: [u8; 0] = [];

    let tag_a1 = compute_mac(CbcVariant::Aes128Sha256, &key_a, &header, data, &[]);
    let tag_a2 = compute_mac(CbcVariant::Aes128Sha256, &key_a, &header, data, &[]);
    assert_eq!(tag_a1, tag_a2, "same inputs must produce the same tag");
    assert_eq!(tag_a1.len(), 32, "HMAC-SHA256 tag must be 32 bytes");

    let tag_b = compute_mac(CbcVariant::Aes128Sha256, &key_b, &header, data, &[]);
    assert_ne!(tag_a1, tag_b, "different keys must produce different tags");

    let tag_sha384 = compute_mac(CbcVariant::Aes256Sha384, &key_a, &header, data, &[]);
    assert_eq!(tag_sha384.len(), 48, "HMAC-SHA384 tag must be 48 bytes");
}

#[test]
fn extract_padding_minimal_valid() {
    // padding_length=0 -> exactly one byte of padding, value 0.
    let payload = [1, 2, 3, 0];
    let (to_remove, good) = extract_padding(&payload);
    assert_eq!(good, 0xff);
    assert_eq!(to_remove, 1);
}

#[test]
fn extract_padding_multi_byte_valid() {
    // padding_length=2 -> three bytes of padding, each value 2.
    let payload = [9, 9, 2, 2, 2];
    let (to_remove, good) = extract_padding(&payload);
    assert_eq!(good, 0xff);
    assert_eq!(to_remove, 3);
}

#[test]
fn extract_padding_rejects_inconsistent_bytes() {
    // Last byte claims padding_length=2 but the preceding byte isn't 2.
    let payload = [9, 9, 5, 2];
    let (_to_remove, good) = extract_padding(&payload);
    assert_eq!(good, 0x00);
}

#[test]
fn extract_padding_rejects_overlong_claim() {
    // Last byte claims padding_length=250 but payload is far shorter.
    let payload = [1, 2, 3, 250];
    let (_to_remove, good) = extract_padding(&payload);
    assert_eq!(good, 0x00);
}

#[test]
fn cbc_block_roundtrip_aes128() {
    let key = [0x11u8; 16];
    let iv = [0x22u8; 16];
    let plaintext = [0xABu8; 32]; // 2 blocks, already aligned (NoPadding requires this)
    let ciphertext = cbc_encrypt(CbcVariant::Aes128Sha256, &key, &iv, &plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len());
    assert_ne!(ciphertext, plaintext);
    let decrypted = cbc_decrypt(CbcVariant::Aes128Sha256, &key, &iv, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn cbc_block_roundtrip_aes256() {
    let key = [0x33u8; 32];
    let iv = [0x44u8; 16];
    let plaintext = [0xCDu8; 48]; // 3 blocks
    let ciphertext = cbc_encrypt(CbcVariant::Aes256Sha384, &key, &iv, &plaintext).unwrap();
    assert_eq!(ciphertext.len(), plaintext.len());
    let decrypted = cbc_decrypt(CbcVariant::Aes256Sha384, &key, &iv, &ciphertext).unwrap();
    assert_eq!(decrypted, plaintext);
}

/// Full record-layer round trip using two `CbcCipher` instances that share
/// the same (synthetic) mac/enc keys -- simulating what two real TLS
/// peers would each derive from the same key block, WITHOUT needing a
/// real handshake. This isolates the record-layer logic (framing, MAC,
/// padding) from the key-schedule slicing in `tls12::ConnectionSecrets`.
#[test]
fn cbc_cipher_full_record_roundtrip() {
    let mac_key = vec![0x55u8; CbcVariant::Aes128Sha256.mac_key_len()];
    let enc_key = vec![0x66u8; CbcVariant::Aes128Sha256.enc_key_len()];

    let mut encrypter = CbcCipher {
        variant: CbcVariant::Aes128Sha256,
        mac_key: mac_key.clone(),
        enc_key: enc_key.clone(),
    };
    let mut decrypter = CbcCipher {
        variant: CbcVariant::Aes128Sha256,
        mac_key,
        enc_key,
    };

    let plaintext = b"hello from the CBC record-layer roundtrip test";
    let msg = OutboundPlainMessage {
        typ: rustls::ContentType::ApplicationData,
        version: rustls::ProtocolVersion::TLSv1_2,
        payload: rustls::crypto::cipher::OutboundChunks::from(&plaintext[..]),
    };
    let seq = 42u64;
    let opaque = encrypter.encrypt(msg, seq).expect("encrypt should succeed");

    let encoded = opaque.encode();
    // encoded = 5-byte record header + ciphertext; feed the ciphertext
    // portion back in as an InboundOpaqueMessage the same way the real
    // deframer would.
    let mut body = encoded[5..].to_vec();
    let inbound = InboundOpaqueMessage::new(
        rustls::ContentType::ApplicationData,
        rustls::ProtocolVersion::TLSv1_2,
        &mut body,
    );
    let plain = decrypter
        .decrypt(inbound, seq)
        .expect("decrypt should succeed and MAC should verify");
    assert_eq!(plain.payload, &plaintext[..]);
}

#[test]
fn cbc_cipher_rejects_tampered_ciphertext() {
    let mac_key = vec![0x77u8; CbcVariant::Aes256Sha384.mac_key_len()];
    let enc_key = vec![0x88u8; CbcVariant::Aes256Sha384.enc_key_len()];

    let mut encrypter = CbcCipher {
        variant: CbcVariant::Aes256Sha384,
        mac_key: mac_key.clone(),
        enc_key: enc_key.clone(),
    };
    let mut decrypter = CbcCipher {
        variant: CbcVariant::Aes256Sha384,
        mac_key,
        enc_key,
    };

    let plaintext = b"do not tamper with me";
    let msg = OutboundPlainMessage {
        typ: rustls::ContentType::ApplicationData,
        version: rustls::ProtocolVersion::TLSv1_2,
        payload: rustls::crypto::cipher::OutboundChunks::from(&plaintext[..]),
    };
    let opaque = encrypter.encrypt(msg, 7).expect("encrypt should succeed");
    let encoded = opaque.encode();
    let mut body = encoded[5..].to_vec();
    // Flip a bit well inside the ciphertext (past the explicit IV).
    let flip_at = body.len() - 5;
    body[flip_at] ^= 0x01;

    let inbound = InboundOpaqueMessage::new(
        rustls::ContentType::ApplicationData,
        rustls::ProtocolVersion::TLSv1_2,
        &mut body,
    );
    let result = decrypter.decrypt(inbound, 7);
    assert!(result.is_err(), "tampered ciphertext must be rejected");
}

#[test]
fn ffdhe2048_key_exchange_roundtrip() {
    use rustls::crypto::SupportedKxGroup;
    let a = FFDHE2048_KX_GROUP.start().expect("side A start");
    let b = FFDHE2048_KX_GROUP.start().expect("side B start");
    let a_pub = a.pub_key().to_vec();
    let b_pub = b.pub_key().to_vec();
    let sec_a = a.complete(&b_pub).expect("side A complete");
    let sec_b = b.complete(&a_pub).expect("side B complete");
    assert_eq!(sec_a.secret_bytes(), sec_b.secret_bytes());
    assert_eq!(sec_a.secret_bytes().len(), rustls::ffdhe_groups::FFDHE2048.p.len());
}
