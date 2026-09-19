// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NIST CAVP known-answer tests for the AES-GCM primitives backing
//! `javax.crypto.Cipher.getInstance("AES/GCM/NoPadding")`.
//!
//! Vectors are taken from the NIST CAVP `gcmEncryptIntel256.rsp` and
//! `gcmEncryptExtIV128.rsp` files (FIPS 197 + SP 800-38D). They cover:
//!
//!  * empty plaintext / empty AAD          — tag computed over key-stream only
//!  * 16-byte plaintext / empty AAD        — single-block CTR + GHASH
//!  * AES-256 with a non-trivial vector    — exercises 14-round key expansion
//!  * tag tamper                           — decrypt must fail with
//!                                          `CryptoError::AuthenticationFailed`
//!  * encrypt/decrypt round-trip on 256    — verifies the inverse path on the
//!                                          AES-256 key schedule
//!
//! The tests bypass `Cipher.getInstance` dispatch (which requires a
//! full VM context) and call the primitive `AesGcm::encrypt` /
//! `AesGcm::decrypt` entry points directly — those are the very
//! functions `cipher_do_final_impl` delegates to once the
//! `<clinit>` shim has cleared the Cipher class-init chain. A failure
//! here would surface in the WP6.3 CipherProbe end-to-end, but the
//! KAT lets us catch a regression before it propagates that far.
//!
//! NOTE: When the C18 RustCrypto migration completes, the production
//! path will switch from the in-tree `crypto_impl` to the `aes-gcm`
//! crate. These KAT vectors will still pass against either backend
//! (both implement NIST SP 800-38D); we keep the test pointed at
//! `crypto_impl` so the legacy synthetic feature continues to be
//! covered.

// `crypto_impl` is the always-compiled real RustCrypto-backed backend (a
// top-level module since the `--no-default-features` fix); the legacy
// `crate::crypto::crypto_impl` re-export only exists with
// `legacy-synthetic-crypto`. Point the KAT at the unconditional path so it
// runs in the default (synthetic-crypto-free) build.
use cratonvm_native_builtins::crypto_impl::{Aes, AesGcm, CryptoError};

/// Decode an ASCII hex string into a byte vector.
fn hex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex string has odd length: {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("invalid hex"))
        .collect()
}

/// Hex-encode bytes for readable assertion messages.
fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// NIST CAVP — AES-128-GCM, empty plaintext, empty AAD.
///
/// Key   = 00000000000000000000000000000000
/// IV    = 000000000000000000000000
/// PT    = (empty)
/// AAD   = (empty)
/// CT    = (empty)
/// Tag   = 58e2fccefa7e3061367f1d57a4e7455a
#[test]
fn aes_128_gcm_empty_plaintext_empty_aad() {
    let key = vec![0u8; 16];
    let nonce = [0u8; 12];
    let aes = Aes::key_expansion(&key).expect("AES-128 key expansion");

    let out = AesGcm::encrypt(&aes, &nonce, &[], &[]);

    assert!(
        out.ciphertext.is_empty(),
        "ciphertext for empty plaintext must be empty, got {}",
        hex_of(&out.ciphertext)
    );
    assert_eq!(
        hex_of(&out.tag),
        "58e2fccefa7e3061367f1d57a4e7455a",
        "tag mismatch vs NIST CAVP vector"
    );
}

/// NIST CAVP — AES-128-GCM, single all-zero block of plaintext.
///
/// Key = 00000000000000000000000000000000
/// IV  = 000000000000000000000000
/// PT  = 00000000000000000000000000000000
/// AAD = (empty)
/// CT  = 0388dace60b6a392f328c2b971b2fe78
/// Tag = ab6e47d42cec13bdf53a67b21257bddf
#[test]
fn aes_128_gcm_single_zero_block() {
    let key = vec![0u8; 16];
    let nonce = [0u8; 12];
    let pt = vec![0u8; 16];
    let aes = Aes::key_expansion(&key).expect("AES-128 key expansion");

    let out = AesGcm::encrypt(&aes, &nonce, &pt, &[]);

    assert_eq!(
        hex_of(&out.ciphertext),
        "0388dace60b6a392f328c2b971b2fe78",
        "CTR keystream XOR mismatch vs NIST CAVP vector"
    );
    assert_eq!(
        hex_of(&out.tag),
        "ab6e47d42cec13bdf53a67b21257bddf",
        "GHASH/tag mismatch vs NIST CAVP vector"
    );

    // Decrypt round-trip on the same vector — confirms the inverse path
    // agrees with the encrypt path on the same key/IV/AAD.
    let recovered = AesGcm::decrypt(&aes, &nonce, &out.ciphertext, &[], &out.tag)
        .expect("authenticated decrypt must succeed on untampered ciphertext");
    assert_eq!(recovered, pt, "decrypted plaintext mismatch");
}

/// NIST SP 800-38D Appendix B — AES-256-GCM test case 13.
///
/// Key  = 00000000000000000000000000000000_00000000000000000000000000000000
/// IV   = 000000000000000000000000
/// PT   = (empty)
/// AAD  = (empty)
/// CT   = (empty)
/// Tag  = 530f8afbc74536b9a963b4f1c4cb738b
///
/// Exercises the AES-256 key schedule + 14-round encrypt path on a
/// known answer, then round-trips through decrypt to confirm the
/// inverse path agrees on the 256-bit key.
#[test]
fn aes_256_gcm_test_case_13_and_roundtrip() {
    let key = vec![0u8; 32];
    let nonce = [0u8; 12];

    let aes = Aes::key_expansion(&key).expect("AES-256 key expansion");
    let out = AesGcm::encrypt(&aes, &nonce, &[], &[]);

    assert!(
        out.ciphertext.is_empty(),
        "empty plaintext must yield empty ciphertext"
    );
    assert_eq!(
        hex_of(&out.tag),
        "530f8afbc74536b9a963b4f1c4cb738b",
        "AES-256-GCM tag mismatch vs NIST SP 800-38D test case 13"
    );

    // Round-trip an arbitrary 32-byte payload so the 14-round decrypt
    // path is exercised too — test case 13 itself has empty PT, so the
    // tag-only check above only covers the GHASH + key-stream-init
    // legs of the algorithm.
    let pt = b"abcdefghijklmnopqrstuvwxyz012345".to_vec();
    let enc = AesGcm::encrypt(&aes, &nonce, &pt, &[]);
    let recovered = AesGcm::decrypt(&aes, &nonce, &enc.ciphertext, &[], &enc.tag)
        .expect("authenticated decrypt must succeed on untampered ciphertext");
    assert_eq!(recovered, pt, "AES-256-GCM round-trip mismatch");
    assert_eq!(enc.ciphertext.len(), pt.len(), "CTR mode preserves length");
    assert_eq!(enc.tag.len(), 16, "GCM tag must be 16 bytes");
}

/// NIST CAVP — AES-128-GCM, single-block vector with the tag flipped.
///
/// Identical setup to `aes_128_gcm_single_zero_block`, except we
/// toggle the low bit of the first tag byte before calling decrypt.
/// The constant-time tag comparison MUST reject it with
/// `CryptoError::AuthenticationFailed` — never with a stale plaintext.
#[test]
fn aes_128_gcm_decrypt_rejects_tag_tamper() {
    let key = vec![0u8; 16];
    let nonce = [0u8; 12];
    let pt = vec![0u8; 16];
    let aes = Aes::key_expansion(&key).expect("AES-128 key expansion");

    let out = AesGcm::encrypt(&aes, &nonce, &pt, &[]);
    let mut bad_tag = out.tag;
    bad_tag[0] ^= 0x01;

    match AesGcm::decrypt(&aes, &nonce, &out.ciphertext, &[], &bad_tag) {
        Err(CryptoError::AuthenticationFailed) => { /* expected */ }
        Err(other) => panic!(
            "expected CryptoError::AuthenticationFailed on tag tamper, got {:?}",
            other
        ),
        Ok(plaintext) => panic!(
            "expected authentication failure on tampered tag, got plaintext {}",
            hex_of(&plaintext)
        ),
    }
}

/// NIST CAVP — AES-128-GCM, single-block vector with the ciphertext
/// flipped instead of the tag.
///
/// Same key/IV/PT as `aes_128_gcm_single_zero_block`. We toggle a
/// single bit in the ciphertext byte stream, leave the original tag
/// intact, and assert decrypt still fails: the GHASH input changed,
/// so the recomputed tag will not match.
#[test]
fn aes_128_gcm_decrypt_rejects_ciphertext_tamper() {
    let key = vec![0u8; 16];
    let nonce = [0u8; 12];
    let pt = vec![0u8; 16];
    let aes = Aes::key_expansion(&key).expect("AES-128 key expansion");

    let out = AesGcm::encrypt(&aes, &nonce, &pt, &[]);
    let mut bad_ct = out.ciphertext.clone();
    bad_ct[0] ^= 0x80;

    let result = AesGcm::decrypt(&aes, &nonce, &bad_ct, &[], &out.tag);
    assert!(
        matches!(result, Err(CryptoError::AuthenticationFailed)),
        "expected AuthenticationFailed on ciphertext tamper, got {:?}",
        result
    );
}
