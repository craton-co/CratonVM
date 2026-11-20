use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// This registrar must claim `java/security/SecureRandom` for exactly two
/// triples, and it must NOT claim either `setSeed` or the seeded ctor.
///
/// L8-securerandom-provider.md. It used to register five, and the extra
/// three were shape-checking no-ops. In `--synthetic-jdk` this registrar is
/// called from `register_synthetic_overrides` AFTER
/// `securerandom::register_random_and_securerandom_natives`, and
/// `register()` is last-registration-wins, so the no-ops silently replaced
/// two working fixes: `securerandom.rs`'s `setSeed` bodies check
/// `secure_random_is_sha1prng` and route SHA1PRNG through real reseeding
/// (`getInstance("SHA1PRNG")` seeded twice alike yields identical bytes on
/// HotSpot — the one replay guarantee the JDK gives a `SecureRandom`), and
/// its `<init>([B)V` stamps the `algorithm` and `provider` fields whose
/// absence is this record's headline defect.
///
/// Asserted as a REGISTRATION census rather than a behavioural check
/// because the defect is a registration: the no-op bodies were each
/// individually defensible, and what made them wrong was which triple they
/// claimed and in what order. A behavioural test would also need a
/// `--synthetic-jdk` VM, which no scheduled corpus run builds.
#[test]
fn crypto_impl_registers_no_securerandom_seeding_triple() {
    let mut r = NativeMethodRegistry::new();
    register_crypto_impl_natives(&mut r);
    let dump = r.dump_registrations();
    let mut mine: Vec<String> = Vec::new();
    for row in dump.iter() {
        if row.0 == "java/security/SecureRandom" {
            mine.push(format!("{}{}", row.1, row.2));
        }
    }
    assert_eq!(
        mine,
        vec![
            "nextBytes([B)V".to_string(),
            "generateSeed(I)[B".to_string()
        ],
        "crypto_impl must own only the two OS-CSPRNG output triples; \
         re-registering setSeed or <init>([B)V here shadows securerandom.rs \
         in synthetic mode and undoes SHA1PRNG reseeding — L8"
    );
    // Stated separately so a future widening of the list above cannot
    // quietly re-admit the two rows this record is about.
    for row in dump.iter() {
        assert!(
            !(row.0 == "java/security/SecureRandom"
                && (row.1 == "setSeed" || row.1 == "<init>")),
            "SecureRandom.setSeed / <init> must be served by securerandom.rs alone"
        );
    }
}

// -----------------------------------------------------------------------
// SHA-256 Tests
// -----------------------------------------------------------------------

#[test]
fn sha256_empty() {
    let h = Sha256::digest(b"");
    assert_eq!(
        hex(&h),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_abc() {
    let h = Sha256::digest(b"abc");
    assert_eq!(
        hex(&h),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sha256_longer_message() {
    let h = Sha256::digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
    assert_eq!(
        hex(&h),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn sha256_incremental() {
    let mut hasher = Sha256::new();
    hasher.update(b"abc");
    hasher.update(b"def");
    let h = hasher.finalize();
    assert_eq!(hex(&h), hex(&Sha256::digest(b"abcdef")));
}

#[test]
fn sha256_single_byte() {
    let h = Sha256::digest(b"a");
    assert_eq!(
        hex(&h),
        "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
    );
}

#[test]
fn sha256_56_bytes() {
    // Exactly the boundary where padding fits in one block
    let data = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnop";
    let h = Sha256::digest(data);
    // Just verify it produces 32 bytes
    assert_eq!(h.len(), 32);
}

#[test]
fn sha256_multi_block() {
    // 128 bytes = 2 blocks exactly
    let data = vec![0x61u8; 128];
    let h = Sha256::digest(&data);
    assert_eq!(h.len(), 32);
}

// -----------------------------------------------------------------------
// SHA-512 Tests
// -----------------------------------------------------------------------

#[test]
fn sha512_empty() {
    let h = Sha512::digest(b"");
    assert_eq!(
        hex(&h),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
}

#[test]
fn sha512_abc() {
    let h = Sha512::digest(b"abc");
    assert_eq!(
        hex(&h),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
}

#[test]
fn sha512_longer() {
    let h = Sha512::digest(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu");
    assert_eq!(
        hex(&h),
        "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
    );
}

// -----------------------------------------------------------------------
// SHA-384 Tests
// -----------------------------------------------------------------------

#[test]
fn sha384_empty() {
    let h = Sha384::digest(b"");
    assert_eq!(
        hex(&h),
        "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"
    );
}

#[test]
fn sha384_abc() {
    let h = Sha384::digest(b"abc");
    assert_eq!(
        hex(&h),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
    );
}

// -----------------------------------------------------------------------
// AES Key Expansion Tests
// -----------------------------------------------------------------------

#[test]
fn aes_key_expansion_128() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    assert_eq!(aes_key.nr, 10);
    assert!(matches!(aes_key.cipher, AesCipher::Aes128(_)));
}

#[test]
fn aes_key_expansion_192() {
    let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
    let aes_key = Aes::key_expansion(&key).unwrap();
    assert_eq!(aes_key.nr, 12);
    assert!(matches!(aes_key.cipher, AesCipher::Aes192(_)));
}

#[test]
fn aes_key_expansion_256() {
    let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let aes_key = Aes::key_expansion(&key).unwrap();
    assert_eq!(aes_key.nr, 14);
    assert!(matches!(aes_key.cipher, AesCipher::Aes256(_)));
}

#[test]
fn aes_key_expansion_invalid() {
    assert!(Aes::key_expansion(&[0u8; 15]).is_err());
    assert!(Aes::key_expansion(&[0u8; 17]).is_err());
}

// -----------------------------------------------------------------------
// AES-128 Encrypt/Decrypt Block Tests (FIPS 197 Appendix B)
// -----------------------------------------------------------------------

#[test]
fn aes128_encrypt_block() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let plaintext = from_hex("3243f6a8885a308d313198a2e0370734");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let mut input = [0u8; 16];
    input.copy_from_slice(&plaintext);
    let ct = Aes::encrypt_block(&aes_key, &input);
    assert_eq!(hex(&ct), "3925841d02dc09fbdc118597196a0b32");
}

#[test]
fn aes128_decrypt_block() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let ciphertext = from_hex("3925841d02dc09fbdc118597196a0b32");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let mut input = [0u8; 16];
    input.copy_from_slice(&ciphertext);
    let pt = Aes::decrypt_block(&aes_key, &input);
    assert_eq!(hex(&pt), "3243f6a8885a308d313198a2e0370734");
}

#[test]
fn aes128_encrypt_decrypt_roundtrip() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let plaintext = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0xff,
    ];
    let ct = Aes::encrypt_block(&aes_key, &plaintext);
    let pt = Aes::decrypt_block(&aes_key, &ct);
    assert_eq!(pt, plaintext);
}

#[test]
fn aes256_encrypt_decrypt_roundtrip() {
    let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let plaintext = [
        0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
        0x17, 0x2a,
    ];
    let ct = Aes::encrypt_block(&aes_key, &plaintext);
    let pt = Aes::decrypt_block(&aes_key, &ct);
    assert_eq!(pt, plaintext);
}

// -----------------------------------------------------------------------
// AES ECB Mode Tests
// -----------------------------------------------------------------------

#[test]
fn aes_ecb_encrypt_decrypt() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let plaintext = b"Hello, World!!!!"; // exactly 16 bytes
    let ct = AesEcb::encrypt(&aes_key, plaintext);
    let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn aes_ecb_empty() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let ct = AesEcb::encrypt(&aes_key, b"");
    // Empty input produces one block of padding
    assert_eq!(ct.len(), 16);
    let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
    assert_eq!(pt, b"");
}

#[test]
fn aes_ecb_multi_block() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let plaintext = b"This is a test of multi-block ECB encryption!";
    let ct = AesEcb::encrypt(&aes_key, plaintext);
    let pt = AesEcb::decrypt(&aes_key, &ct).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn aes_ecb_invalid_ciphertext_length() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    assert!(AesEcb::decrypt(&aes_key, &[0u8; 15]).is_err());
}

// -----------------------------------------------------------------------
// AES CBC Mode Tests
// -----------------------------------------------------------------------

#[test]
fn aes_cbc_encrypt_decrypt() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let iv = from_hex("000102030405060708090a0b0c0d0e0f");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let mut iv_arr = [0u8; 16];
    iv_arr.copy_from_slice(&iv);
    let plaintext = b"Hello CBC Mode!!"; // 16 bytes
    let ct = AesCbc::encrypt(&aes_key, &iv_arr, plaintext);
    let pt = AesCbc::decrypt(&aes_key, &iv_arr, &ct).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn aes_cbc_multi_block() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let iv = [0u8; 16];
    let plaintext =
        b"This is a longer message that spans multiple AES blocks for CBC mode testing.";
    let ct = AesCbc::encrypt(&aes_key, &iv, plaintext);
    let pt = AesCbc::decrypt(&aes_key, &iv, &ct).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn aes_cbc_empty() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let iv = [0u8; 16];
    let ct = AesCbc::encrypt(&aes_key, &iv, b"");
    let pt = AesCbc::decrypt(&aes_key, &iv, &ct).unwrap();
    assert_eq!(pt, b"");
}

#[test]
fn aes_cbc_invalid_ciphertext() {
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let iv = [0u8; 16];
    assert!(AesCbc::decrypt(&aes_key, &iv, &[0u8; 7]).is_err());
}

// -----------------------------------------------------------------------
// AES-GCM Tests
// -----------------------------------------------------------------------

#[test]
fn aes_gcm_encrypt_decrypt_roundtrip() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let plaintext = b"Hello, GCM!";
    let aad = b"additional data";
    let output = AesGcm::encrypt(&aes_key, &nonce, plaintext, aad);
    let pt = AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, aad, &output.tag).unwrap();
    assert_eq!(pt, plaintext);
}

#[test]
fn aes_gcm_empty_plaintext() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let output = AesGcm::encrypt(&aes_key, &nonce, b"", b"");
    assert!(output.ciphertext.is_empty());
    let pt = AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, b"", &output.tag).unwrap();
    assert!(pt.is_empty());
}

#[test]
fn aes_gcm_auth_failure() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let output = AesGcm::encrypt(&aes_key, &nonce, b"test", b"aad");
    // Tamper with tag
    let mut bad_tag = output.tag;
    bad_tag[0] ^= 0xff;
    assert!(AesGcm::decrypt(&aes_key, &nonce, &output.ciphertext, b"aad", &bad_tag).is_err());
}

#[test]
fn aes_gcm_wrong_aad() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let output = AesGcm::encrypt(&aes_key, &nonce, b"test", b"correct aad");
    assert!(AesGcm::decrypt(
        &aes_key,
        &nonce,
        &output.ciphertext,
        b"wrong aad",
        &output.tag
    )
    .is_err());
}

#[test]
fn aes_gcm_tampered_ciphertext() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let output = AesGcm::encrypt(&aes_key, &nonce, b"secret data", b"");
    let mut bad_ct = output.ciphertext.clone();
    if !bad_ct.is_empty() {
        bad_ct[0] ^= 0xff;
    }
    assert!(AesGcm::decrypt(&aes_key, &nonce, &bad_ct, b"", &output.tag).is_err());
}

#[test]
fn aes_gcm_large_plaintext() {
    let key = from_hex("feffe9928665731c6d6a8f9467308308");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = from_hex("cafebabefacedbaddecaf888");
    let mut nonce_arr = [0u8; 12];
    nonce_arr.copy_from_slice(&nonce);
    let plaintext = vec![0xabu8; 256];
    let aad = b"test aad";
    let output = AesGcm::encrypt(&aes_key, &nonce_arr, &plaintext, aad);
    let pt =
        AesGcm::decrypt(&aes_key, &nonce_arr, &output.ciphertext, aad, &output.tag).unwrap();
    assert_eq!(pt, plaintext);
}
// -----------------------------------------------------------------------
// C18 — NIST SP 800-38D AES-128-GCM Known-Answer Tests
//
// Vectors lifted from NIST SP 800-38D Appendix B, Test Case 3 —
// the canonical AES-128-GCM KAT also reproduced in RFC 5288 and
// every published GCM reference implementation. Proves the
// RustCrypto-backed `AesGcm::{encrypt, decrypt}` matches the
// bit-for-bit ciphertext + tag, and that a tampered tag is
// rejected with `AuthenticationFailed`.
// -----------------------------------------------------------------------

#[test]
fn c18_nist_aes128_gcm_test_case_3_encrypt() {
    // K  = feffe9928665731c6d6a8f9467308308
    // P  = d9313225..f391aafd255 (64 bytes)
    // IV = cafebabefacedbaddecaf888 (12 bytes)
    // A  = (empty)
    // C  = 42831ec2..73f5985
    // T  = 4d5c2af327cd64a62cf35abd2ba6fab4
    let key = from_hex("feffe9928665731c6d6a8f9467308308");
    let iv = from_hex("cafebabefacedbaddecaf888");
    let pt = from_hex(
        "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da\
         2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525\
         b16aedf5aa0de657ba637b391aafd255",
    );
    let aad: Vec<u8> = Vec::new();
    let expected_ct = from_hex(
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e0\
         35c17e2329aca12e21d514b25466931c7d8f6a5aac84aa05\
         1ba30b396a0aac973d58e091473f5985",
    );
    let expected_tag = from_hex("4d5c2af327cd64a62cf35abd2ba6fab4");

    let aes_key = Aes::key_expansion(&key).unwrap();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&iv);
    let out = AesGcm::encrypt(&aes_key, &nonce, &pt, &aad);
    assert_eq!(
        hex(&out.ciphertext),
        hex(&expected_ct),
        "AES-128-GCM ciphertext"
    );
    assert_eq!(hex(&out.tag), hex(&expected_tag), "AES-128-GCM tag");
}

#[test]
fn c18_nist_aes128_gcm_test_case_3_decrypt_and_auth_fail() {
    // Same vector — decrypt direction, plus an explicit
    // AuthenticationFailed proof on a tampered tag.
    let key = from_hex("feffe9928665731c6d6a8f9467308308");
    let iv = from_hex("cafebabefacedbaddecaf888");
    let ct = from_hex(
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e0\
         35c17e2329aca12e21d514b25466931c7d8f6a5aac84aa05\
         1ba30b396a0aac973d58e091473f5985",
    );
    let aad: Vec<u8> = Vec::new();
    let tag_bytes = from_hex("4d5c2af327cd64a62cf35abd2ba6fab4");
    let expected_pt = from_hex(
        "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da\
         2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525\
         b16aedf5aa0de657ba637b391aafd255",
    );

    let aes_key = Aes::key_expansion(&key).unwrap();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&iv);
    let mut tag = [0u8; 16];
    tag.copy_from_slice(&tag_bytes);

    let pt = AesGcm::decrypt(&aes_key, &nonce, &ct, &aad, &tag)
        .expect("NIST Test Case 3 decrypt must succeed");
    assert_eq!(hex(&pt), hex(&expected_pt), "AES-128-GCM plaintext");

    // Tamper the high bit of the tag — auth must fail.
    let mut bad_tag = tag;
    bad_tag[0] ^= 0x80;
    let res = AesGcm::decrypt(&aes_key, &nonce, &ct, &aad, &bad_tag);
    assert!(
        matches!(res, Err(CryptoError::AuthenticationFailed)),
        "AES-128-GCM with tampered tag must return AuthenticationFailed, got {:?}",
        res
    );
}

// -----------------------------------------------------------------------
// HMAC Tests (RFC 4231 test vectors)
// -----------------------------------------------------------------------

#[test]
fn hmac_sha256_test_vector_1() {
    // RFC 4231 Test Case 1
    let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let data = b"Hi There";
    let mac = Hmac::mac(&key, data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
}

#[test]
fn hmac_sha256_test_vector_2() {
    // RFC 4231 Test Case 2 — key = "Jefe"
    let key = b"Jefe";
    let data = b"what do ya want for nothing?";
    let mac = Hmac::mac(key, data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn hmac_sha256_test_vector_3() {
    // RFC 4231 Test Case 3
    let key = from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let data = from_hex("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
    let mac = Hmac::mac(&key, &data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
    );
}

#[test]
fn hmac_sha384_test_vector_1() {
    let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let data = b"Hi There";
    let mac = Hmac::mac(&key, data, HashFunction::Sha384);
    assert_eq!(
        hex(&mac),
        "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6"
    );
}

#[test]
fn hmac_sha512_test_vector_1() {
    let key = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let data = b"Hi There";
    let mac = Hmac::mac(&key, data, HashFunction::Sha512);
    assert_eq!(
        hex(&mac),
        "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
    );
}

#[test]
fn hmac_incremental() {
    let key = b"key";
    let mut h = Hmac::new(key, HashFunction::Sha256);
    h.update(b"hello ");
    h.update(b"world");
    let mac1 = h.finalize();
    let mac2 = Hmac::mac(key, b"hello world", HashFunction::Sha256);
    assert_eq!(mac1, mac2);
}

// -----------------------------------------------------------------------
// HKDF Tests (RFC 5869 test vectors)
// -----------------------------------------------------------------------

#[test]
fn hkdf_sha256_test_vector_1() {
    // RFC 5869 Test Case 1
    let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let salt = from_hex("000102030405060708090a0b0c");
    let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
    let okm = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 42);
    assert_eq!(
        hex(&okm),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
}

#[test]
fn hkdf_sha256_test_vector_2() {
    // RFC 5869 Test Case 2
    let ikm = from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f");
    let salt = from_hex("606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeaf");
    let info = from_hex("b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
    let okm = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 82);
    assert_eq!(
        hex(&okm),
        "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87"
    );
}

#[test]
fn hkdf_sha256_test_vector_3() {
    // RFC 5869 Test Case 3 — zero-length salt and info
    let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let okm = Hkdf::derive(HashFunction::Sha256, &[], &ikm, &[], 42);
    assert_eq!(
        hex(&okm),
        "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
    );
}

#[test]
fn hkdf_extract_expand_separate() {
    let ikm = from_hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let salt = from_hex("000102030405060708090a0b0c");
    let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
    let prk = Hkdf::extract(HashFunction::Sha256, &salt, &ikm);
    let okm = Hkdf::expand(HashFunction::Sha256, &prk, &info, 42);
    let okm_combined = Hkdf::derive(HashFunction::Sha256, &salt, &ikm, &info, 42);
    assert_eq!(okm, okm_combined);
}

// -----------------------------------------------------------------------
// SecureRandom Tests
// -----------------------------------------------------------------------

#[test]
fn secure_random_deterministic() {
    let mut rng1 = SecureRandom::new_with_seed(42);
    let mut rng2 = SecureRandom::new_with_seed(42);
    let mut buf1 = [0u8; 32];
    let mut buf2 = [0u8; 32];
    rng1.next_bytes(&mut buf1);
    rng2.next_bytes(&mut buf2);
    assert_eq!(buf1, buf2);
}

#[test]
fn secure_random_different_seeds() {
    let mut rng1 = SecureRandom::new_with_seed(42);
    let mut rng2 = SecureRandom::new_with_seed(43);
    assert_ne!(rng1.next_u64(), rng2.next_u64());
}

#[test]
fn secure_random_next_u32() {
    let mut rng = SecureRandom::new_with_seed(1);
    let v1 = rng.next_u32();
    let v2 = rng.next_u32();
    assert_ne!(v1, v2);
}

#[test]
fn secure_random_fill_buffer() {
    let mut rng = SecureRandom::new_with_seed(99);
    let mut buf = [0u8; 100];
    rng.next_bytes(&mut buf);
    // Not all zeros
    assert!(buf.iter().any(|&b| b != 0));
}

#[test]
fn secure_random_sequential_not_equal() {
    let mut rng = SecureRandom::new_with_seed(7);
    let v1 = rng.next_u64();
    let v2 = rng.next_u64();
    let v3 = rng.next_u64();
    assert_ne!(v1, v2);
    assert_ne!(v2, v3);
}

// VULN(secrand) / VULN(secrand-collision) regression: SecureRandom output now
// comes straight from the OS CSPRNG and is INDEPENDENT of the `key` argument
// (the old identity-hash-keyed DRBG side-table — and its collision/aliasing
// hazard — is gone). Each draw must be fresh regardless of the key.

#[test]
fn secure_random_fill_is_fresh_every_draw() {
    // Successive draws (same key) must differ — no cached/replayed state.
    let mut a1 = [0u8; 32];
    let mut a2 = [0u8; 32];
    secure_random_fill(0x1111_1111, &mut a1);
    secure_random_fill(0x1111_1111, &mut a2);
    assert_ne!(a1, a2, "successive draws must be fresh, not cached");
    // Different key value — also fresh, never aliased to the first key.
    let mut b1 = [0u8; 32];
    secure_random_fill(0x2222_2222, &mut b1);
    assert_ne!(a1, b1, "draws must not be correlated across keys");
    // Output must not be all-zero (no degenerate output).
    assert!(a1.iter().any(|&b| b != 0));
}

#[test]
fn secure_random_fill_ignores_key_collisions() {
    // The collision hazard is removed: two draws with the SAME key value
    // (the worst case an identity-hash collision could produce) are still
    // independent fresh OS draws, not a shared/aliased deterministic stream.
    let key = 0x3333_3333;
    let mut first = [0u8; 32];
    let mut second = [0u8; 32];
    secure_random_fill(key, &mut first);
    secure_random_fill(key, &mut second);
    assert_ne!(
        first, second,
        "same-key draws must remain independent (no aliased DRBG state)"
    );
}

// -----------------------------------------------------------------------
// PKCS7 Padding Tests
// -----------------------------------------------------------------------

#[test]
fn pkcs7_pad_full_block() {
    let padded = pkcs7_pad(b"1234567890123456", 16);
    // 16 bytes input -> 32 bytes with full block of padding
    assert_eq!(padded.len(), 32);
    assert!(padded[16..].iter().all(|&b| b == 16));
}

#[test]
fn pkcs7_pad_partial() {
    let padded = pkcs7_pad(b"hello", 16);
    assert_eq!(padded.len(), 16);
    assert!(padded[5..].iter().all(|&b| b == 11));
}

#[test]
fn pkcs7_unpad_valid() {
    let mut data = b"hello".to_vec();
    data.extend(std::iter::repeat(11u8).take(11));
    let unpadded = pkcs7_unpad(&data).unwrap();
    assert_eq!(unpadded, b"hello");
}

#[test]
fn pkcs7_unpad_invalid() {
    assert!(pkcs7_unpad(&[]).is_err());
    assert!(pkcs7_unpad(&[0u8; 16]).is_err()); // pad byte = 0 is invalid
}

// -----------------------------------------------------------------------
// CryptoError Display Tests
// -----------------------------------------------------------------------

#[test]
fn crypto_error_display() {
    assert!(format!("{}", CryptoError::InvalidKeyLength(7)).contains("7"));
    assert!(format!("{}", CryptoError::InvalidBlockSize).contains("block"));
    assert!(format!("{}", CryptoError::InvalidPadding).contains("padding"));
    assert!(format!("{}", CryptoError::AuthenticationFailed).contains("authentication"));
    assert!(format!("{}", CryptoError::InvalidNonceLength).contains("nonce"));
    assert!(format!("{}", CryptoError::UnsupportedAlgorithm("foo".into())).contains("foo"));
}

// -----------------------------------------------------------------------
// HashFunction Tests
// -----------------------------------------------------------------------

#[test]
fn hash_function_block_sizes() {
    assert_eq!(HashFunction::Sha256.block_size(), 64);
    assert_eq!(HashFunction::Sha384.block_size(), 128);
    assert_eq!(HashFunction::Sha512.block_size(), 128);
}

#[test]
fn hash_function_output_sizes() {
    assert_eq!(HashFunction::Sha256.output_size(), 32);
    assert_eq!(HashFunction::Sha384.output_size(), 48);
    assert_eq!(HashFunction::Sha512.output_size(), 64);
}

#[test]
fn hash_function_hash_sha256() {
    let h = HashFunction::Sha256.hash(b"abc");
    assert_eq!(hex(&h), hex(&Sha256::digest(b"abc")));
}

#[test]
fn hash_function_hash_sha512() {
    let h = HashFunction::Sha512.hash(b"abc");
    assert_eq!(hex(&h), hex(&Sha512::digest(b"abc").to_vec()));
}

// -----------------------------------------------------------------------
// NIST AES-128 ECB Known Answer Test
// -----------------------------------------------------------------------

#[test]
fn aes128_ecb_nist_vector() {
    // NIST SP 800-38A F.1.1 ECB-AES128.Encrypt
    let key = from_hex("2b7e151628aed2a6abf7158809cf4f3c");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
    let mut block = [0u8; 16];
    block.copy_from_slice(&pt);
    let ct = Aes::encrypt_block(&aes_key, &block);
    assert_eq!(hex(&ct), "3ad77bb40d7a3660a89ecaf32466ef97");
}

// -----------------------------------------------------------------------
// Splitmix64 Tests
// -----------------------------------------------------------------------

#[test]
fn splitmix64_deterministic() {
    let mut s1 = 42u64;
    let mut s2 = 42u64;
    assert_eq!(splitmix64(&mut s1), splitmix64(&mut s2));
}

// -----------------------------------------------------------------------
// ChaCha20 keystream (SecureRandom OS-entropy fallback) — RFC 7539 vector
// -----------------------------------------------------------------------

#[test]
fn chacha20_keystream_rfc7539_zero_key() {
    // RFC 7539 §2.3.2 derived: ChaCha20 keystream for an all-zero 256-bit
    // key, all-zero 96-bit nonce, starting block counter 0. This is the
    // canonical "ChaCha20 of zeros" reference output.
    let key = [0u8; 32];
    let nonce = [0u8; 12];
    let mut out = [0u8; 64];
    chacha20_keystream_fill(&key, &nonce, &mut out);
    assert_eq!(
        hex(&out),
        "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
         da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
    );
}

/// The vectors below were MEASURED, not recalled: HotSpot 25's own SunJCE
/// `Cipher.getInstance("ChaCha20")` was initialised with the stated key,
/// `ChaCha20ParameterSpec(nonce, counter)`, and asked to encrypt an
/// all-zero plaintext — which yields the raw keystream. Encrypting zeros is
/// the only way to read a stream cipher's keystream through the JCA API,
/// and it makes the oracle's output directly comparable with
/// `chacha20_xor` over a zero buffer.
///
/// The zero-key vector above (`chacha20_keystream_rfc7539_zero_key`) was
/// re-derived from the same oracle in passing and matches this tree's
/// existing expectation byte for byte, which is independent evidence that
/// the pre-existing core was already correct — what it lacked was a
/// caller-supplied counter and an XOR, not a working permutation.
#[test]
fn chacha20_xor_counter_one_matches_hotspot() {
    // key = 00..1f, nonce = 00 00 00 00 00 00 00 4a 00 00 00 00 (RFC 8439
    // §2.4.2's key and nonce), block counter 1.
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut out = [0u8; 128];
    chacha20_xor(&key, &nonce, 1, &mut out);
    assert_eq!(
        hex(&out),
        "224f51f3401bd9e12fde276fb8631ded8c131f823d2c06e27e4fcaec9ef3cf78\
         8a3b0aa372600a92b57974cded2b9334794cba40c63e34cdea212c4cf07d41b7\
         69a6749f3f630f4122cafe28ec4dc47e26d4346d70b98c73f3e9c53ac40c5945\
         398b6eda1a832c89c167eacd901d7e2bf363740373201aa188fbbce83991c4ed"
    );
}

/// The counter is a plain block index, and this is the observation that
/// proves it without trusting either implementation: the same key and nonce
/// at counter 0 must produce, as its SECOND 64-byte block, exactly what
/// counter 1 produces as its FIRST. HotSpot's output has this property;
/// so must ours. A core that ignored `initial_counter` would still pass
/// the vector test above if its expectation were taken from itself — this
/// one it could not pass.
#[test]
fn chacha20_xor_counter_is_a_block_index() {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut from_zero = [0u8; 128];
    chacha20_xor(&key, &nonce, 0, &mut from_zero);
    let mut from_one = [0u8; 64];
    chacha20_xor(&key, &nonce, 1, &mut from_one);
    assert_eq!(&from_zero[64..], &from_one[..]);
    // …and the counter-0 stream is HotSpot's, so neither block is ours alone.
    assert_eq!(
        hex(&from_zero[..64]),
        "af051e40bba0354981329a806a140eafd258a22a6dcb4bb9f6569cb3efe2deaf\
         837bd87ca20b5ba12081a306af0eb35c41a239d20dfc74c81771560d9c9c1e4b"
    );
}

/// A length that is not a multiple of the 64-byte block must stop mid-block
/// and must not touch the bytes past the end. HotSpot's 70-byte answer is
/// the 128-byte answer truncated, which is the property being pinned.
#[test]
fn chacha20_xor_partial_final_block() {
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = i as u8;
    }
    let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
    let mut out = [0u8; 70];
    chacha20_xor(&key, &nonce, 1, &mut out);
    assert_eq!(
        hex(&out),
        "224f51f3401bd9e12fde276fb8631ded8c131f823d2c06e27e4fcaec9ef3cf78\
         8a3b0aa372600a92b57974cded2b9334794cba40c63e34cdea212c4cf07d41b7\
         69a6749f3f63"
    );
}

/// XOR, not write: running the same keystream over the same buffer twice
/// must restore the plaintext. This is the property that makes the function
/// a cipher rather than a generator, and the one the old
/// `copy_from_slice` body did not have.
#[test]
fn chacha20_xor_is_an_involution() {
    let key = [0x5au8; 32];
    let nonce = [0x3cu8; 12];
    let plaintext: Vec<u8> = (0..200u32).map(|i| (i * 31) as u8).collect();
    let mut buf = plaintext.clone();
    chacha20_xor(&key, &nonce, 7, &mut buf);
    assert_ne!(buf, plaintext, "ciphertext must differ from plaintext");
    chacha20_xor(&key, &nonce, 7, &mut buf);
    assert_eq!(buf, plaintext, "decrypting must restore the plaintext");
}

#[test]
fn chacha20_keystream_spans_multiple_blocks() {
    // A request larger than one 64-byte block must keep advancing the
    // counter: the second block differs from the first (no repetition).
    let key = [7u8; 32];
    let nonce = [3u8; 12];
    let mut out = [0u8; 128];
    chacha20_keystream_fill(&key, &nonce, &mut out);
    assert_ne!(&out[..64], &out[64..], "blocks must not repeat");
}

// -----------------------------------------------------------------------
// AES-256 NIST test vectors
// -----------------------------------------------------------------------

#[test]
fn aes256_nist_vector() {
    let key = from_hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
    let mut block = [0u8; 16];
    block.copy_from_slice(&pt);
    let ct = Aes::encrypt_block(&aes_key, &block);
    assert_eq!(hex(&ct), "f3eed1bdb5d2a03c064b5a7e3db181f8");
}

// -----------------------------------------------------------------------
// GCM with AAD only (no plaintext)
// -----------------------------------------------------------------------

#[test]
fn aes_gcm_aad_only() {
    let key = from_hex("00000000000000000000000000000000");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let nonce = [0u8; 12];
    let output = AesGcm::encrypt(&aes_key, &nonce, b"", b"some aad data");
    assert!(output.ciphertext.is_empty());
    // Tag should still be non-zero
    assert!(output.tag.iter().any(|&b| b != 0));
    let pt = AesGcm::decrypt(&aes_key, &nonce, b"", b"some aad data", &output.tag).unwrap();
    assert!(pt.is_empty());
}

// -----------------------------------------------------------------------
// HMAC with long key (> block size)
// -----------------------------------------------------------------------

#[test]
fn hmac_sha256_long_key() {
    // RFC 4231 Test Case 6: key longer than block size
    let key = vec![0xaa; 131];
    let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
    let mac = Hmac::mac(&key, data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn hmac_sha256_long_key_and_data() {
    // RFC 4231 Test Case 7
    let key = vec![0xaa; 131];
    let data = b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.";
    let mac = Hmac::mac(&key, data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
    );
}

// -----------------------------------------------------------------------
// HKDF with SHA-512
// -----------------------------------------------------------------------

#[test]
fn hkdf_sha512_basic() {
    let ikm = vec![0x0b; 22];
    let salt = from_hex("000102030405060708090a0b0c");
    let info = from_hex("f0f1f2f3f4f5f6f7f8f9");
    let okm = Hkdf::derive(HashFunction::Sha512, &salt, &ikm, &info, 42);
    // Just verify it produces 42 bytes
    assert_eq!(okm.len(), 42);
}

// -----------------------------------------------------------------------
// AES-192 Tests
// -----------------------------------------------------------------------

#[test]
fn aes192_encrypt_decrypt_roundtrip() {
    let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let pt = [
        0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
        0x17, 0x2a,
    ];
    let ct = Aes::encrypt_block(&aes_key, &pt);
    let decrypted = Aes::decrypt_block(&aes_key, &ct);
    assert_eq!(decrypted, pt);
}

#[test]
fn aes192_nist_vector() {
    let key = from_hex("8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b");
    let aes_key = Aes::key_expansion(&key).unwrap();
    let pt = from_hex("6bc1bee22e409f96e93d7e117393172a");
    let mut block = [0u8; 16];
    block.copy_from_slice(&pt);
    let ct = Aes::encrypt_block(&aes_key, &block);
    assert_eq!(hex(&ct), "bd334f1d6e45f25ff712a214571fa5cc");
}

// =======================================================================
// BigUint tests
// =======================================================================

#[test]
fn biguint_basic_arithmetic() {
    let a = BigUint::from_u64(12345);
    let b = BigUint::from_u64(6789);
    let sum = a.add(&b);
    assert_eq!(sum.to_bytes_be(), BigUint::from_u64(19134).to_bytes_be());
    let diff = a.sub(&b);
    assert_eq!(diff.to_bytes_be(), BigUint::from_u64(5556).to_bytes_be());
    let prod = a.mul(&b);
    assert_eq!(
        prod.to_bytes_be(),
        BigUint::from_u64(12345 * 6789).to_bytes_be()
    );
}

#[test]
fn biguint_div_rem() {
    let a = BigUint::from_u64(1000000);
    let b = BigUint::from_u64(7);
    let (q, r) = a.div_rem(&b);
    assert_eq!(q.to_bytes_be(), BigUint::from_u64(142857).to_bytes_be());
    assert_eq!(r.to_bytes_be(), BigUint::from_u64(1).to_bytes_be());
}

/// Build a `BigUint` from little-endian limbs, so a test can aim at an
/// exact internal shape rather than hoping a decimal literal lands on one.
fn biguint_from_limbs(limbs: &[u32]) -> BigUint {
    let mut n = BigUint {
        limbs: limbs.to_vec(),
    };
    n.normalize();
    n
}

/// `u = q*v + r` and `0 <= r < v`, the only thing division has to promise.
fn assert_div_rem_identity(u: &BigUint, v: &BigUint) {
    let (q, r) = u.div_rem(v);
    assert_eq!(
        q.mul(v).add(&r).to_bytes_be(),
        u.to_bytes_be(),
        "q*v + r != u for u={:?} v={:?} (q={:?} r={:?})",
        u.limbs,
        v.limbs,
        q.limbs,
        r.limbs
    );
    assert_eq!(
        r.cmp(v),
        std::cmp::Ordering::Less,
        "remainder {:?} not < divisor {:?}",
        r.limbs,
        v.limbs
    );
}

/// Knuth D's quotient-digit estimate must not overflow, on the branch
/// that can only be reached from inside a division.
///
/// `r_hat` is bounded by `base` only on the `u_high < v_top` branch. On
/// the other one it starts at `u_mid + v_top`, which reaches ~2^33, and
/// the old loop computed `base * r_hat` before testing whether `r_hat` was
/// in range at all. `2^32 * 2^33` does not fit: debug builds panicked,
/// **release builds wrapped** and skipped a correction that was due.
///
/// Reaching this through `div_rem` means steering a *partial remainder*
/// several digits in — a 400,000-pair random sweep never did it once, and
/// neither did three full RSA keygen/encrypt/decrypt cycles. Calling the
/// estimate directly makes it three arguments.
///
/// Each case is checked against the definition of the estimate rather than
/// a hard-coded answer: `q_hat` must be the true quotient digit or exactly
/// one more, which is all D3 promises and all D4/D6 need.
#[test]
fn qhat_estimate_is_exact_or_one_high_without_overflowing() {
    const BASE: u128 = 1 << 32;
    // `u_high == v_top` — the branch that overflowed — plus `u_high` above
    // and below it, at the extremes of `u_mid`/`u_low`/`v_top2`.
    let v_tops = [0x8000_0000u64, 0xFFFF_FFFF, 0xC000_0001];
    let extremes = [0u64, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF];
    for v_top in v_tops {
        for v_top2 in extremes {
            for u_mid in extremes {
                for u_low in extremes {
                    for u_high in [v_top, v_top - 1, v_top.saturating_sub(0x1234_5678), 0] {
                        let q_hat =
                            estimate_quotient_digit(u_high, u_mid, u_low, v_top, v_top2);
                        assert!(q_hat < BASE as u64, "q_hat must fit a limb: {q_hat:#x}");
                        // The true digit, computed in 128-bit with the full
                        // three-limb numerator and two-limb divisor.
                        let numerator =
                            ((u_high as u128) << 64) | ((u_mid as u128) << 32) | u_low as u128;
                        let divisor = ((v_top as u128) << 32) | v_top2 as u128;
                        let exact = numerator / divisor;
                        let exact = exact.min(BASE - 1);
                        assert!(
                            q_hat as u128 == exact || q_hat as u128 == exact + 1,
                            "q_hat {q_hat:#x} is neither the true digit {exact:#x} nor one more \
                             (u_high={u_high:#x} u_mid={u_mid:#x} u_low={u_low:#x} \
                             v_top={v_top:#x} v_top2={v_top2:#x})"
                        );
                    }
                }
            }
        }
    }
}

/// The exact operands that overflowed: `u_high == v_top` with `u_mid`
/// large enough that `u_mid + v_top` crosses 2^32. Before the fix this
/// panicked with "attempt to multiply with overflow" in a debug build.
#[test]
fn qhat_estimate_survives_the_operands_that_panicked() {
    for v_top in [0x8000_0000u64, 0xABCD_EF01, 0xFFFF_FFFF] {
        for u_mid in [0xFFFF_FFFFu64, 0x8000_0000, 0xC000_0000] {
            let q_hat = estimate_quotient_digit(v_top, u_mid, 0xFFFF_FFFF, v_top, 0xFFFF_FFFF);
            assert!(q_hat < 1u64 << 32);
        }
    }
}

/// The `div_rem` shapes the estimate feeds, end to end.
#[test]
fn biguint_div_rem_qhat_estimate_does_not_overflow() {
    let v = biguint_from_limbs(&[0x0000_0001, 0x8000_0000]);
    for u_mid in [0x8000_0000u32, 0xFFFF_FFFF, 0xC000_0000] {
        for u_low in [0u32, 1, 0xFFFF_FFFF] {
            let u = biguint_from_limbs(&[u_low, u_mid, 0x8000_0000]);
            assert_div_rem_identity(&u, &v);
            // And with a trailing limb, so the loop runs for more than one
            // quotient digit and `u` has been mutated by D4 before the
            // estimate is made again.
            let u = biguint_from_limbs(&[u_low, u_mid, 0x8000_0000, 0x7FFF_FFFF]);
            assert_div_rem_identity(&u, &v);
        }
    }
}

/// Sweep divisor/dividend shapes deterministically.
///
/// The defect above reached production as an intermittent failure of
/// `rsa_cipher_roundtrip_all_paddings`, because whether it fires depends
/// on the limb values of a randomly generated key — so the coverage that
/// would have caught it cannot itself depend on random input. This walks a
/// fixed LCG instead: same pairs on every run, on every machine.
#[test]
fn biguint_div_rem_identity_over_many_shapes() {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 32) as u32
    };
    for v_limbs in 2..=6usize {
        for u_limbs in v_limbs..=10usize {
            for _ in 0..40 {
                let v: Vec<u32> = (0..v_limbs).map(|_| next()).collect();
                let u: Vec<u32> = (0..u_limbs).map(|_| next()).collect();
                let v = biguint_from_limbs(&v);
                let u = biguint_from_limbs(&u);
                if v.is_zero() {
                    continue;
                }
                assert_div_rem_identity(&u, &v);
            }
        }
    }
}

/// The extremes of the estimate: an all-ones divisor, a divisor whose top
/// limb is the smallest value that still normalises to itself, and top
/// limbs that make `q_hat` land on `base - 1`.
#[test]
fn biguint_div_rem_extreme_limb_values() {
    let cases: &[(&[u32], &[u32])] = &[
        (
            &[0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF],
            &[0xFFFF_FFFF, 0xFFFF_FFFF],
        ),
        (&[0, 0, 0xFFFF_FFFF], &[0xFFFF_FFFF, 0x8000_0000]),
        (&[0xFFFF_FFFF, 0, 0x8000_0000], &[0, 0x8000_0000]),
        (&[1, 0xFFFF_FFFF, 0xFFFF_FFFF], &[0xFFFF_FFFF, 0xFFFF_FFFF]),
        (&[0, 0, 0, 1], &[1, 1]),
        (&[0xFFFF_FFFF; 8], &[0x0000_0001, 0x8000_0000]),
    ];
    for (u, v) in cases {
        assert_div_rem_identity(&biguint_from_limbs(u), &biguint_from_limbs(v));
    }
}

#[test]
fn biguint_modpow() {
    // 3^13 mod 50 = 1594323 mod 50 = 23
    let base = BigUint::from_u64(3);
    let exp = BigUint::from_u64(13);
    let m = BigUint::from_u64(50);
    let result = base.modpow(&exp, &m);
    assert_eq!(result.to_bytes_be(), BigUint::from_u64(23).to_bytes_be());
}

#[test]
fn biguint_modinv() {
    // 3^-1 mod 11 = 4 (since 3*4 = 12 ≡ 1 mod 11)
    let a = BigUint::from_u64(3);
    let m = BigUint::from_u64(11);
    let inv = a.modinv(&m).unwrap();
    assert_eq!(inv.to_bytes_be(), BigUint::from_u64(4).to_bytes_be());
}

#[test]
fn biguint_bytes_roundtrip() {
    let bytes = from_hex("deadbeef01020304050607080910111213141516");
    let n = BigUint::from_bytes_be(&bytes);
    let out = n.to_bytes_be();
    assert_eq!(out, bytes);
}

// =======================================================================
// RSA tests
// =======================================================================

#[test]
fn rsa_sign_verify_1024() {
    let (pub_key, priv_key) = Rsa::generate_keypair(1024);
    assert!(pub_key.n.bit_length() == 1024);
    let message = b"Hello RSA!";
    let sig = Rsa::sign_sha256(&priv_key, message);
    assert!(Rsa::verify_sha256(&pub_key, message, &sig));
    // Tamper with message
    assert!(!Rsa::verify_sha256(&pub_key, b"wrong", &sig));
}

#[test]
fn rsa_key_serialization() {
    let (pub_key, priv_key) = Rsa::generate_keypair(1024);
    let der = Rsa::public_key_to_der(&pub_key);
    assert!(!der.is_empty());
    // Parse it back
    let parsed = parse_rsa_public_key(&der).unwrap();
    assert_eq!(parsed.n.to_bytes_be(), pub_key.n.to_bytes_be());
    assert_eq!(parsed.e.to_bytes_be(), pub_key.e.to_bytes_be());

    let priv_der = Rsa::private_key_to_der(&priv_key);
    assert!(!priv_der.is_empty());
}

/// A generated key MUST carry its CRT parameters and emit the complete
/// 9-element PKCS#1 `RSAPrivateKey`, not the legacy 4-element non-CRT form.
/// Regression guard for http-server-sslengine-identity-singleton-clobber:
/// a non-CRT DER round-tripped through `RSAKeyFactory$Legacy` becomes a
/// `sun.security.rsa.RSAPrivateKeyImpl` whose `getEncoded()` is the
/// incomplete 572-byte PKCS#8 that rustls rejects.
#[test]
fn rsa_generated_key_is_crt() {
    let (_pub_key, priv_key) = Rsa::generate_keypair(1024);
    let p = priv_key.p.as_ref().expect("p present");
    let q = priv_key.q.as_ref().expect("q present");
    let dp = priv_key.dp.as_ref().expect("dp present");
    let dq = priv_key.dq.as_ref().expect("dq present");
    let qinv = priv_key.qinv.as_ref().expect("qinv present");
    // p > q (JDK convention).
    assert_eq!(p.cmp(q), std::cmp::Ordering::Greater, "p must exceed q");
    // n == p*q.
    assert_eq!(p.mul(q).to_bytes_be(), priv_key.n.to_bytes_be(), "n == p*q");
    // dp == d mod (p-1), dq == d mod (q-1).
    let p1 = p.sub(&BigUint::one());
    let q1 = q.sub(&BigUint::one());
    assert_eq!(priv_key.d.modulo(&p1).to_bytes_be(), dp.to_bytes_be(), "dP");
    assert_eq!(priv_key.d.modulo(&q1).to_bytes_be(), dq.to_bytes_be(), "dQ");
    // qinv*q ≡ 1 (mod p).
    assert_eq!(
        qinv.mul(q).modulo(p).to_bytes_be(),
        BigUint::one().to_bytes_be(),
        "qInv*q ≡ 1 mod p"
    );
    // The DER carries the CRT tail: a 1024-bit CRT RSAPrivateKey is ~600+
    // bytes; the non-CRT (n,e,d)-only form is ~350. Assert we're on the
    // CRT side of that gap.
    let der = Rsa::private_key_to_der(&priv_key);
    assert!(
        der.len() > 500,
        "generated RSA-1024 key DER must be the full CRT form (got {} bytes)",
        der.len()
    );
}

/// A key with no CRT params (an imported bare `(n, d)`) still round-trips
/// through the legacy 4-element form — no panic, no CRT tail.
#[test]
fn rsa_noncrt_key_emits_legacy_form() {
    let key = RsaPrivateKey {
        n: BigUint::from_bytes_be(&[0x00, 0xC1, 0x00, 0x01]),
        d: BigUint::from_bytes_be(&[0x03]),
        e: BigUint::from_bytes_be(&[0x01, 0x00, 0x01]),
        p: None,
        q: None,
        dp: None,
        dq: None,
        qinv: None,
    };
    let der = Rsa::private_key_to_der(&key);
    assert!(!der.is_empty());
}

// =======================================================================
// ECDSA P-256 tests
// =======================================================================

#[test]
fn p256_generator_on_curve() {
    let g = Ecdsa::generator();
    // Verify y^2 = x^3 - 3x + b mod p
    let p = &P256_P;
    let x2 = FieldElement256::mul_mod(&g.x, &g.x, p);
    let x3 = FieldElement256::mul_mod(&x2, &g.x, p);
    let three = FieldElement256 {
        limbs: [3, 0, 0, 0],
    };
    let three_x = FieldElement256::mul_mod(&three, &g.x, p);
    let rhs = FieldElement256::add_mod(&FieldElement256::sub_mod(&x3, &three_x, p), &P256_B, p);
    let y2 = FieldElement256::mul_mod(&g.y, &g.y, p);
    assert_eq!(y2.limbs, rhs.limbs);
}

#[test]
fn ecdsa_sign_verify() {
    let (pub_key, priv_key) = Ecdsa::generate_keypair();
    let msg = b"test message for ECDSA";
    let sig = Ecdsa::sign_sha384(&priv_key, msg);
    assert!(Ecdsa::verify_sha384(&pub_key, msg, &sig));
    assert!(!Ecdsa::verify_sha384(&pub_key, b"wrong", &sig));
}

#[test]
fn ecdsa_key_serialization() {
    let (pub_key, _priv_key) = Ecdsa::generate_keypair();
    let bytes = Ecdsa::public_key_to_bytes(&pub_key);
    assert_eq!(bytes.len(), 65);
    assert_eq!(bytes[0], 0x04);
    let parsed = Ecdsa::public_key_from_bytes(&bytes).unwrap();
    assert_eq!(parsed.point.x.limbs, pub_key.point.x.limbs);
    assert_eq!(parsed.point.y.limbs, pub_key.point.y.limbs);
}

// =======================================================================
// DER encoding/decoding tests
// =======================================================================

#[test]
fn der_integer_encode_decode() {
    let val = vec![0x01, 0x00, 0x01]; // 65537
    let encoded = der_encode_integer(&val);
    assert_eq!(encoded[0], 0x02); // INTEGER tag
    let (decoded, _) = der_read_integer(&encoded).unwrap();
    assert_eq!(decoded, val);
}

#[test]
fn der_ecdsa_sig_roundtrip() {
    let r = vec![0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
    let s = vec![0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
    let encoded = der_encode_ecdsa_signature(&r, &s);
    let (r2, s2) = der_decode_ecdsa_signature(&encoded).unwrap();
    assert_eq!(r2, r);
    assert_eq!(s2, s);
}

// =======================================================================
// JKS KeyStore tests
// =======================================================================

#[test]
fn jks_magic_detection() {
    let data = vec![
        0xFE, 0xED, 0xFE, 0xED, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
    ];
    let ks = KeyStoreData::load_jks(&data, b"").unwrap();
    assert_eq!(ks.store_type, "JKS");
    assert_eq!(ks.entries.len(), 0);
}

#[test]
fn jks_invalid_magic() {
    let data = vec![0x00, 0x00, 0x00, 0x00];
    assert!(KeyStoreData::load_jks(&data, b"").is_err());
}

#[test]
fn keystore_auto_detect() {
    let jks = vec![
        0xFE, 0xED, 0xFE, 0xED, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
    ];
    let ks = KeyStoreData::load(&jks, b"", "auto").unwrap();
    assert_eq!(ks.store_type, "JKS");
}

// =======================================================================
// X.509 certificate parsing tests
// =======================================================================

#[test]
fn x509_parse_self_signed() {
    // Build a minimal self-signed X.509 cert structure for testing
    // This is a structurally valid DER cert (not cryptographically valid)
    let serial = der_encode_integer(&[0x01]);
    // Sig alg: sha256WithRSAEncryption
    let sig_alg_oid: &[u8] = &[
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00,
    ];
    let sig_alg = der_encode_sequence(sig_alg_oid);
    // Issuer: CN=Test
    let cn_oid = &[0x06, 0x03, 0x55, 0x04, 0x03];
    let cn_val = &[0x0c, 0x04, 0x54, 0x65, 0x73, 0x74]; // UTF8String "Test"
    let mut atv = Vec::new();
    atv.extend_from_slice(cn_oid);
    atv.extend_from_slice(cn_val);
    let atv_seq = der_encode_sequence(&atv);
    let rdn_set = {
        let mut s = vec![0x31];
        s.extend_from_slice(&der_encode_length(atv_seq.len()));
        s.extend_from_slice(&atv_seq);
        s
    };
    let issuer = der_encode_sequence(&rdn_set);
    let subject = issuer.clone();
    // Validity: notBefore=240101000000Z, notAfter=341231235959Z
    let nb = &[
        0x17, 0x0d, b'2', b'4', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0',
        b'Z',
    ];
    let na = &[
        0x17, 0x0d, b'3', b'4', b'1', b'2', b'3', b'1', b'2', b'3', b'5', b'9', b'5', b'9',
        b'Z',
    ];
    let mut validity_inner = Vec::new();
    validity_inner.extend_from_slice(nb);
    validity_inner.extend_from_slice(na);
    let validity = der_encode_sequence(&validity_inner);
    // SPKI (stub RSA)
    let spki =
        der_encode_sequence(&[0x30, 0x03, 0x06, 0x01, 0x00, 0x03, 0x03, 0x00, 0x01, 0x02]);

    // TBSCertificate
    let version = &[0xa0, 0x03, 0x02, 0x01, 0x02]; // v3
    let mut tbs = Vec::new();
    tbs.extend_from_slice(version);
    tbs.extend_from_slice(&serial);
    tbs.extend_from_slice(&sig_alg);
    tbs.extend_from_slice(&issuer);
    tbs.extend_from_slice(&validity);
    tbs.extend_from_slice(&subject);
    tbs.extend_from_slice(&spki);
    let tbs_seq = der_encode_sequence(&tbs);

    // Outer sig alg + signature
    let outer_sig_alg = sig_alg.clone();
    let sig_value = &[0x03, 0x03, 0x00, 0xab, 0xcd]; // BIT STRING

    let mut cert_inner = Vec::new();
    cert_inner.extend_from_slice(&tbs_seq);
    cert_inner.extend_from_slice(&outer_sig_alg);
    cert_inner.extend_from_slice(sig_value);
    let cert_der = der_encode_sequence(&cert_inner);

    let parsed = X509Cert::parse_der(&cert_der).unwrap();
    assert_eq!(parsed.version, 3);
    assert_eq!(parsed.sig_algorithm, "SHA256withRSA");
    assert_eq!(parsed.subject_cn, "Test");
    assert_eq!(parsed.issuer_cn, "Test");
    assert!(parsed.not_before > 0);
    assert!(parsed.not_after > parsed.not_before);
}

#[test]
fn datetime_to_epoch_known_value() {
    // 2024-01-01 00:00:00 UTC = 1704067200
    let ts = super::datetime_to_epoch(2024, 1, 1, 0, 0, 0);
    assert_eq!(ts, 1704067200);
}

// =======================================================================
// Global store tests
// =======================================================================

#[test]
fn keystore_global_store_roundtrip() {
    let id = keystore_next_id();
    let data = KeyStoreData {
        store_type: "JKS".into(),
        entries: HashMap::new(),
    };
    keystore_store(id, data);
    let retrieved = keystore_get(id).unwrap();
    assert_eq!(retrieved.store_type, "JKS");
}

// --- Phase 80.4: SecureRandom CSPRNG Tests ---

#[test]
fn os_random_bytes_succeeds() {
    // OS entropy must succeed on any platform where this JVM runs.
    let mut buf = [0u8; 32];
    assert!(
        super::os_random_bytes(&mut buf),
        "OS entropy source must be available"
    );
    // Output must not be all zeros (statistically impossible for 32 bytes).
    assert_ne!(buf, [0u8; 32], "OS entropy must produce non-zero output");
}

#[test]
fn secure_random_output_distribution() {
    // Verify that SecureRandom produces non-trivial output.
    // Generate 256 bytes and check that at least 8 distinct byte values appear.
    let mut sr = super::SecureRandom::new();
    let mut buf = [0u8; 256];
    sr.next_bytes(&mut buf);
    let mut seen = std::collections::HashSet::new();
    for &b in &buf {
        seen.insert(b);
    }
    assert!(
        seen.len() >= 8,
        "256 random bytes must contain at least 8 distinct values, got {}",
        seen.len()
    );
}

// -----------------------------------------------------------------------
// Phase F — additional JCA conformance coverage
// -----------------------------------------------------------------------

// RF.1: NIST-published digest test vectors for MD5 / SHA-1 / SHA-512.
// SHA-256 is already covered above; these complete the "four algorithms"
// success criterion from the roadmap.

#[test]
fn rf1_sha1_abc() {
    // FIPS PUB 180-1 Appendix A
    let h = crate::real_sha1(b"abc");
    assert_eq!(hex(&h), "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn rf1_sha1_longer_message() {
    // FIPS PUB 180-1 Appendix A
    let h = crate::real_sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
    assert_eq!(hex(&h), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
}

#[test]
fn rf1_md5_empty_and_known() {
    // RFC 1321 test suite
    assert_eq!(
        hex(&crate::real_md5(b"")),
        "d41d8cd98f00b204e9800998ecf8427e"
    );
    assert_eq!(
        hex(&crate::real_md5(b"abc")),
        "900150983cd24fb0d6963f7d28e17f72"
    );
    assert_eq!(
        hex(&crate::real_md5(b"message digest")),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );
}

#[test]
fn rf1_sha512_abc() {
    // FIPS PUB 180-4 Appendix C
    let h = Sha512::digest(b"abc");
    assert_eq!(
        hex(&h),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
         2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
}

// RF.2: HMAC-SHA256/384/512 RFC 4231 Test Case 2 (key=Jefe). Test Case 1
// is covered above; Case 2 probes a different key/data ratio and was
// chosen because the original suite already covers the all-aa key case.

#[test]
fn rf2_hmac_sha256_rfc4231_tc2() {
    // RFC 4231 §4.3 Test Case 2
    let key = b"Jefe";
    let data = b"what do ya want for nothing?";
    let mac = Hmac::mac(key, data, HashFunction::Sha256);
    assert_eq!(
        hex(&mac),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn rf2_hmac_sha512_rfc4231_tc2() {
    let key = b"Jefe";
    let data = b"what do ya want for nothing?";
    let mac = Hmac::mac(key, data, HashFunction::Sha512);
    assert_eq!(
        hex(&mac),
        "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
         9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
    );
}

// RF.3: NIST SP 800-38D GCM test vector — the roadmap asks for AES-256-GCM
// parity with NIST vectors. Test Case 1 from gcmEncryptExtIV256.rsp
// (zero key, zero IV, zero plaintext) is the canonical smoke test.
#[test]
fn rf3_aes_256_gcm_nist_zero_vector() {
    use aes_gcm::{aead::Aead, Aes256Gcm, Key, KeyInit, Nonce};
    let key = Key::<Aes256Gcm>::from_slice(&[0u8; 32]);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&[0u8; 12]);
    let ct = cipher.encrypt(nonce, b"".as_ref()).unwrap();
    // Expected tag (no plaintext) from NIST vectors.
    assert_eq!(hex(&ct), "530f8afbc74536b9a963b4f1c4cb738b");
}

/// Attribution probe for the RSA private-key operation, kept because
/// `perf/biginteger-modpow-has-no-montgomery-reduction-20260817` was closed
/// on the strength of it: after Montgomery, the secret-exponent modpow is
/// no longer the whole cost of a signature, and anyone tempted to make
/// modpow faster again should re-run this first and check that modpow is
/// still what they are paying for.
///
/// `cargo test --release -p cratonvm-native-builtins --lib -- --ignored \
///     rsa_sign_cost_attribution --nocapture`
#[test]
#[ignore = "timing probe, not an assertion; run explicitly with --nocapture"]
fn rsa_sign_cost_attribution() {
    use std::time::Instant;
    let (_pk, sk) = Rsa::generate_keypair(2048);
    let msg = b"the floor under every certificate test";

    let t = Instant::now();
    for _ in 0..10 {
        let _ = Rsa::sign_sha256(&sk, msg);
    }
    let whole = t.elapsed();

    let m = BigUint::from_bytes_be(&Sha256::digest(msg));
    let t = Instant::now();
    for _ in 0..10 {
        let _ = m.modpow(&sk.d, &sk.n);
    }
    let secret_modpow = t.elapsed();

    let t = Instant::now();
    for _ in 0..10 {
        let _ = rsa_crt_exponentiate(&sk, &m).expect("CRT");
    }
    let crt = t.elapsed();

    let r = rsa_random_coprime(&sk.n).expect("blinding factor");
    let t = Instant::now();
    for _ in 0..10 {
        let _ = r.modinv(&sk.n);
    }
    let modinv = t.elapsed();

    let t = Instant::now();
    for _ in 0..10 {
        let _ = r.modpow(&sk.e, &sk.n);
    }
    let public_modpow = t.elapsed();

    let t = Instant::now();
    for _ in 0..10 {
        let _ = rsa_random_coprime(&sk.n);
    }
    let draw = t.elapsed();

    println!("--- RSA-2048 sign_sha256, x10 ---");
    println!("  whole signature      {whole:>12.2?}");
    println!("  secret-exponent modpow {secret_modpow:>10.2?}   (full-width d)");
    println!("  CRT + fault check    {crt:>12.2?}");
    println!("  r.modinv(n)          {modinv:>12.2?}");
    println!("  r^e mod n            {public_modpow:>12.2?}");
    println!("  draw blinding factor {draw:>12.2?}");
}

/// `BigUint::modpow` splits on the parity of the modulus: odd takes the
/// Montgomery ladder, even keeps dividing. The two arms must be
/// indistinguishable — this drives them against each other over odd moduli
/// of every limb width, and both against a `u128` oracle on small operands
/// where an exact answer can be computed independently.
///
/// The doc this retires
/// (`perf/biginteger-modpow-has-no-montgomery-reduction-20260817`) names the
/// failure mode precisely: a subtly wrong modPow yields plausible-looking
/// wrong signatures that still round-trip. Checking the Montgomery arm only
/// against itself would not catch that; checking it against the routine it
/// replaced does.
#[test]
fn modpow_montgomery_matches_the_dividing_ladder() {
    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    // Small operands against an exact u128 oracle.
    for m in [3u64, 5, 7, 255, 65537, 4294967295, 4294967291] {
        for base in [0u64, 1, 2, 3, m - 1, m, m + 1, 123456789] {
            for e in [0u64, 1, 2, 3, 17, 65537, 4294967296] {
                let got = BigUint::from_u64(base)
                    .modpow(&BigUint::from_u64(e), &BigUint::from_u64(m));
                let mut want: u128 = 1;
                let mut b = (base % m) as u128;
                let mut k = e;
                while k > 0 {
                    if k & 1 == 1 {
                        want = want * b % m as u128;
                    }
                    b = b * b % m as u128;
                    k >>= 1;
                }
                assert_eq!(
                    got.to_bytes_be(),
                    BigUint::from_u64(want as u64).to_bytes_be(),
                    "{base}^{e} mod {m}"
                );
            }
        }
    }

    // Wide operands: Montgomery arm vs the dividing arm it replaced.
    let mut state = 0x51ed_c0de_0817_2026u64;
    for limbs in 1..=8usize {
        for _ in 0..12 {
            let mut m = BigUint {
                limbs: (0..limbs).map(|_| xorshift(&mut state) as u32).collect(),
            };
            m.limbs[0] |= 1; // odd
            m.limbs[limbs - 1] |= 1 << 31; // full width
            let base = BigUint {
                limbs: (0..limbs).map(|_| xorshift(&mut state) as u32).collect(),
            };
            for ebits in [1usize, 7, 32, 33, 64, 127, 256] {
                let ewords = ebits.div_ceil(32);
                let mut e = BigUint {
                    limbs: (0..ewords).map(|_| xorshift(&mut state) as u32).collect(),
                };
                let top = (ebits - 1) % 32;
                e.limbs[ewords - 1] &= (1u32 << top) | ((1u32 << top) - 1);
                e.limbs[ewords - 1] |= 1u32 << top;
                assert_eq!(
                    base.modpow(&e, &m).limbs,
                    base.modpow_dividing(&e, &m).limbs,
                    "montgomery vs dividing, {limbs} limbs, {ebits}-bit exponent"
                );
            }
            // Degenerate bases and exponents.
            let zero = BigUint::zero();
            assert_eq!(base.modpow(&zero, &m).limbs, vec![1], "x^0 == 1");
            assert_eq!(
                zero.modpow(&BigUint::from_u64(5), &m).limbs,
                Vec::<u32>::new()
            );
            assert_eq!(
                base.modpow(&BigUint::one(), &m).limbs,
                base.modulo(&m).limbs
            );
            // An even modulus must still take the dividing arm and agree
            // with itself.
            let m_even = m.add(&BigUint::one());
            let e = BigUint::from_u64(65537);
            assert_eq!(
                base.modpow(&e, &m_even).limbs,
                base.modpow_dividing(&e, &m_even).limbs,
                "even modulus falls back"
            );
        }
    }
}

/// The CRT private op against the full-width one it replaced.
///
/// `internal/performance/rsa-private-key-op-crt-FIXED-20260817`
/// set the bar: "a differential test against the non-CRT path across key
/// sizes, imported-key (`None` CRT params) fallback, and the fault-check
/// branch". All three are here, and the differential is against
/// `base.modpow(&d, &n)` — the exact expression the CRT path replaced — so
/// the new arm is never checked only against itself.
#[test]
fn crt_private_op_matches_the_full_width_exponentiation() {
    for bits in [1024usize, 2048] {
        let (_pk, sk) = Rsa::generate_keypair(bits);
        assert!(sk.p.is_some(), "generated key must carry CRT parameters");

        // A spread of bases, including the ones that exercise the
        // `m1 < m2` borrow in Garner and the degenerate residues.
        let mut bases = vec![
            BigUint::zero(),
            BigUint::one(),
            BigUint::from_u64(2),
            BigUint::from_u64(65537),
            sk.n.sub(&BigUint::one()),
        ];
        let mut seed = 0x5eed_c27_0817u64;
        for _ in 0..24 {
            let mut bytes = vec![0u8; bits / 8];
            for b in bytes.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *b = seed as u8;
            }
            bases.push(BigUint::from_bytes_be(&bytes).modulo(&sk.n));
        }

        for base in &bases {
            let want = base.modpow(&sk.d, &sk.n);
            let crt = rsa_crt_exponentiate(&sk, base).expect("CRT must engage");
            assert_eq!(
                crt.to_bytes_be(),
                want.to_bytes_be(),
                "{bits}-bit CRT disagrees with full-width d"
            );
            // The dispatcher must pick the same value either way.
            assert_eq!(
                rsa_private_exponentiate(&sk, base).to_bytes_be(),
                want.to_bytes_be(),
                "{bits}-bit dispatcher"
            );
            // Blinded, the answer is still the answer — the blinding factor
            // is random per call, so this also says the unblind is right.
            assert_eq!(
                rsa_private_op_blinded(&sk, base).to_bytes_be(),
                want.to_bytes_be(),
                "{bits}-bit blinded CRT"
            );
        }
    }
}

/// A key with no CRT parameters — every `(n, d)` import — must still sign,
/// via the full-width fallback. Losing the parameters may cost speed; it
/// may never change an answer.
#[test]
fn crt_absent_falls_back_and_still_signs() {
    let (pk, sk) = Rsa::generate_keypair(1024);
    let stripped = RsaPrivateKey {
        n: sk.n.clone(),
        d: sk.d.clone(),
        e: sk.e.clone(),
        p: None,
        q: None,
        dp: None,
        dq: None,
        qinv: None,
    };
    let base = BigUint::from_u64(0xdead_beef);
    assert!(
        rsa_crt_exponentiate(&stripped, &base).is_none(),
        "no parameters => no CRT"
    );
    assert_eq!(
        rsa_private_exponentiate(&stripped, &base).to_bytes_be(),
        base.modpow(&sk.d, &sk.n).to_bytes_be(),
        "fallback must equal the full-width answer"
    );
    let msg = b"imported key, no CRT tail";
    let sig = Rsa::sign_sha256(&stripped, msg);
    assert!(
        Rsa::verify_sha256(&pk, msg, &sig),
        "stripped key still signs"
    );

    // Partial parameters are not usable parameters: any missing member
    // sends it back to the fallback rather than half-computing.
    for drop_idx in 0..5 {
        let mut k = clone_key(&sk);
        match drop_idx {
            0 => k.p = None,
            1 => k.q = None,
            2 => k.dp = None,
            3 => k.dq = None,
            _ => k.qinv = None,
        }
        assert!(
            rsa_crt_exponentiate(&k, &base).is_none(),
            "missing member {drop_idx} must decline CRT"
        );
        assert_eq!(
            rsa_private_exponentiate(&k, &base).to_bytes_be(),
            base.modpow(&sk.d, &sk.n).to_bytes_be(),
            "missing member {drop_idx} still answers correctly"
        );
    }
}

/// The Bellcore branch: a key whose CRT parameters are WRONG must be caught
/// by the fault check and fall back, never return the faulty value.
///
/// This is the whole reason the check exists. A faulty CRT signature lets
/// `gcd(s - s_correct, n)` recover a factor of `n` from a single sample,
/// and `parse_rsa_private_key_der` reads all five parameters out of a
/// PKCS#8 file without ever checking `n == p*q`, so a hostile key file
/// reaches this code directly.
#[test]
fn corrupt_crt_parameters_are_caught_and_do_not_leak_a_factor() {
    let (_pk, sk) = Rsa::generate_keypair(1024);
    let base = BigUint::from_u64(0x1234_5678_9abc);
    let correct = base.modpow(&sk.d, &sk.n);

    // Corrupt each parameter in turn. `dP`/`dQ` are the realistic fault
    // (a flipped exponent bit); `p`/`q`/`qInv` cover a malformed import.
    for name in ["dP", "dQ", "qInv", "p", "q"] {
        let mut bad = clone_key(&sk);
        let one = BigUint::one();
        let two = BigUint::from_u64(2);
        match name {
            "dP" => bad.dp = Some(bad.dp.as_ref().unwrap().add(&one)),
            "dQ" => bad.dq = Some(bad.dq.as_ref().unwrap().add(&one)),
            "qInv" => bad.qinv = Some(bad.qinv.as_ref().unwrap().add(&one)),
            "p" => bad.p = Some(bad.p.as_ref().unwrap().add(&two)),
            _ => bad.q = Some(bad.q.as_ref().unwrap().add(&two)),
        }
        assert!(
            rsa_crt_exponentiate(&bad, &base).is_none(),
            "corrupt {name} must fail the fault check"
        );
        // And the caller-facing entry points still return the RIGHT answer,
        // because declining CRT falls back rather than failing.
        assert_eq!(
            rsa_private_exponentiate(&bad, &base).to_bytes_be(),
            correct.to_bytes_be(),
            "corrupt {name}: fallback answer"
        );
        assert_eq!(
            rsa_private_op_blinded(&bad, &base).to_bytes_be(),
            correct.to_bytes_be(),
            "corrupt {name}: blinded fallback answer"
        );
    }

    // A degenerate p or q declines before any arithmetic runs.
    for degenerate in [BigUint::zero(), BigUint::one()] {
        let mut bad = clone_key(&sk);
        bad.p = Some(degenerate.clone());
        assert!(rsa_crt_exponentiate(&bad, &base).is_none(), "degenerate p");
        let mut bad = clone_key(&sk);
        bad.q = Some(degenerate.clone());
        assert!(rsa_crt_exponentiate(&bad, &base).is_none(), "degenerate q");
    }

    // An unverifiable public exponent gets no CRT at all.
    for tiny in [BigUint::zero(), BigUint::one(), BigUint::from_u64(2)] {
        let mut bad = clone_key(&sk);
        bad.e = tiny;
        assert!(
            rsa_crt_exponentiate(&bad, &base).is_none(),
            "e too small to verify => no CRT"
        );
    }
}

fn clone_key(k: &RsaPrivateKey) -> RsaPrivateKey {
    RsaPrivateKey {
        n: k.n.clone(),
        d: k.d.clone(),
        e: k.e.clone(),
        p: k.p.clone(),
        q: k.q.clone(),
        dp: k.dp.clone(),
        dq: k.dq.clone(),
        qinv: k.qinv.clone(),
    }
}

/// The handle-based decrypt (CRT) must be indistinguishable from the
/// `(n, d)` decrypt it accelerates — same plaintexts AND same failure
/// classes. The two share a body precisely so they cannot drift, and this
/// is the test that says so.
#[test]
fn decrypt_by_id_matches_the_n_d_path_including_its_refusals() {
    let (pk, sk) = Rsa::generate_keypair(2048);
    let id = rsa_key_next_id();
    rsa_key_store(
        id,
        RsaKeyPairData {
            public_key: pk,
            private_key: sk,
        },
    );
    let (n, d) = rsa_key_get_priv(id).expect("private components");
    let (_, e) = rsa_key_get_pub(id).expect("public components");

    for pad in [
        RsaCipherPadding::Pkcs1,
        RsaCipherPadding::OaepSha1,
        RsaCipherPadding::OaepSha256,
    ] {
        // A ciphertext with a leading zero byte makes the "short" row below
        // hand decrypt the same integer; re-roll, as the sibling test does.
        let ct = loop {
            let c = rsa_cipher_encrypt(&n, &e, pad, b"crt payload").expect("encrypt");
            if c[0] != 0 {
                break c;
            }
        };

        let by_id = rsa_cipher_decrypt_by_id(id, &n, pad, &ct)
            .expect("handle is in the store")
            .expect("decrypt");
        let by_nd = rsa_cipher_decrypt(&n, &d, pad, &ct).expect("decrypt");
        assert_eq!(by_id, b"crt payload".to_vec(), "{pad:?}: plaintext");
        assert_eq!(by_id, by_nd, "{pad:?}: the two paths must agree");

        // Refusals must carry the same class from both entry points.
        let mut corrupt = ct.clone();
        corrupt[200] ^= 0x01;
        let a = rsa_cipher_decrypt_by_id(id, &n, pad, &corrupt)
            .expect("handle")
            .expect_err("corrupt must not decrypt");
        let b = rsa_cipher_decrypt(&n, &d, pad, &corrupt).expect_err("corrupt");
        assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: corrupt class");
        assert_eq!(a.message(), b.message(), "{pad:?}: corrupt message");

        let mut long = ct.clone();
        long.push(0);
        let a = rsa_cipher_decrypt_by_id(id, &n, pad, &long)
            .expect("handle")
            .expect_err("over-long must be refused");
        let b = rsa_cipher_decrypt(&n, &d, pad, &long).expect_err("over-long");
        assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: over-long class");
        assert_eq!(a.message(), b.message(), "{pad:?}: over-long message");

        let a = rsa_cipher_decrypt_by_id(id, &n, pad, &ct[1..])
            .expect("handle")
            .expect_err("short must not decrypt");
        let b = rsa_cipher_decrypt(&n, &d, pad, &ct[1..]).expect_err("short");
        assert_eq!(a.jca_class(), b.jca_class(), "{pad:?}: short class");
    }

    // An unknown handle is "I cannot help", not a decryption failure — the
    // caller falls back to (n, d) rather than reporting a bad ciphertext.
    assert!(
        rsa_cipher_decrypt_by_id(u64::MAX, &n, RsaCipherPadding::Pkcs1, b"x").is_none(),
        "unknown handle must decline, not fail"
    );

    // THE INTERLOCK. A handle that names a real but DIFFERENT key must
    // decline rather than decrypt with it. This is the collision the JCA
    // layer's field-slot fallback can produce on a genuine JDK key, and
    // without the modulus check it would silently return a wrong plaintext.
    let (other_pk, other_sk) = Rsa::generate_keypair(2048);
    let other_id = rsa_key_next_id();
    rsa_key_store(
        other_id,
        RsaKeyPairData {
            public_key: other_pk,
            private_key: other_sk,
        },
    );
    let ct =
        rsa_cipher_encrypt(&n, &e, RsaCipherPadding::Pkcs1, b"crt payload").expect("encrypt");
    assert!(
        rsa_cipher_decrypt_by_id(other_id, &n, RsaCipherPadding::Pkcs1, &ct).is_none(),
        "a handle naming a different key must decline"
    );
    // ...while the right handle for that same modulus still works.
    assert_eq!(
        rsa_cipher_decrypt_by_id(id, &n, RsaCipherPadding::Pkcs1, &ct)
            .expect("handle")
            .expect("decrypt"),
        b"crt payload".to_vec()
    );
}

// RF.6: RSA key generation produces a functional sign/verify pair.
#[test]
fn rf6_rsa_keypair_roundtrip() {
    let (pk, sk) = Rsa::generate_keypair(1024);
    let msg = b"phase-f roadmap verification";
    let sig = Rsa::sign_sha256(&sk, msg);
    assert!(Rsa::verify_sha256(&pk, msg, &sig), "own-key verify");
    // Tampered payload must fail.
    let mut bad = msg.to_vec();
    bad[0] ^= 1;
    assert!(!Rsa::verify_sha256(&pk, &bad, &sig), "tampered payload");
}

#[test]
fn rsa_blinding_matches_plain_modpow() {
    // Base blinding MUST be functionally transparent: the blinded private
    // exponentiation has to produce exactly the same result as the plain
    // `modpow` for every operand. Use the textbook RSA instance
    // (n=3233, e=17, d=2753) so the math is checkable by hand, plus a sweep
    // of operands to catch any reduction/inverse mistake.
    let n = BigUint::from_u64(3233);
    let e = BigUint::from_u64(17);
    let d = BigUint::from_u64(2753);
    for base_v in [2u64, 7, 42, 65, 1000, 3232] {
        let base = BigUint::from_u64(base_v);
        let expected = base.modpow(&d, &n);
        // With public exponent (RSA sign path).
        let with_e = rsa_private_modpow_blinded(&base, &d, &e, &n);
        assert!(
            with_e == expected,
            "blinded(with e) mismatch for base {}",
            base_v
        );
        // Without public exponent (RSA Cipher decrypt path).
        let no_e = rsa_private_modpow_blinded_no_e(&base, &d, &n);
        assert!(
            no_e == expected,
            "blinded(no e) mismatch for base {}",
            base_v
        );
    }
}

#[test]
fn rsa_random_coprime_is_in_range_and_coprime() {
    // The blinding factor must be coprime to n (so it is invertible) and a
    // non-trivial value in [2, n).
    let n = BigUint::from_u64(3233); // 53 * 61
    let r = rsa_random_coprime(&n).expect("OS entropy available in test");
    assert!(
        r.cmp(&BigUint::from_u64(2)) != std::cmp::Ordering::Less,
        "r >= 2"
    );
    assert!(r.cmp(&n) == std::cmp::Ordering::Less, "r < n");
    let (g, _, _, _, _) = BigUint::extended_gcd(&r, &n);
    assert!(g.is_one(), "gcd(r, n) == 1");
}

// RSA `Cipher` round-trip: encrypt with the public key, decrypt with the
// private key, for every supported padding. Exercises the EME-PKCS1-v1_5
// and EME-OAEP (SHA-1 / SHA-256) encode/decode paths used by the keycloak
// JWE RSA1_5 / RSA-OAEP / RSA-OAEP-256 transformations. Also covers the
// base-blinded decrypt path (blinding must not change the plaintext).
#[test]
fn rsa_cipher_roundtrip_all_paddings() {
    let (pk, sk) = Rsa::generate_keypair(2048);
    let n = pk.n.to_bytes_be();
    let e = pk.e.to_bytes_be();
    let d = sk.d.to_bytes_be();
    for pad in [
        RsaCipherPadding::Pkcs1,
        RsaCipherPadding::OaepSha1,
        RsaCipherPadding::OaepSha256,
    ] {
        let msg = b"0123456789abcdef"; // 16-byte AES CEK, like JWE
        let ct = rsa_cipher_encrypt(&n, &e, pad, msg).expect("encrypt");
        assert_eq!(ct.len(), 256, "{:?}: ciphertext is modulus-sized", pad);
        let pt = rsa_cipher_decrypt(&n, &d, pad, &ct).expect("decrypt");
        assert_eq!(pt, msg, "{:?}: round-trip", pad);
    }
}

/// The exception CLASS every RSA failure mode must become, pinned against
/// SunJCE on Temurin 25.0.3+9 (`probes/JcaExceptionTypeProbe.expected.txt`).
///
/// Every one of these used to be an unchecked `IllegalStateException` at the
/// `Cipher` layer, so a caller's `catch (BadPaddingException e)` — the
/// exception `doFinal` DECLARES — was dead. The class is the only thing a
/// `catch` selects on, so the class is what this asserts; the round-trip
/// test above cannot see any of it, because a round trip never fails.
#[test]
fn rsa_cipher_failures_carry_the_class_sunjce_raises() {
    // ORDER THE TWO MODULI, and the reason is a measured 1-in-8 flake.
    //
    // The wrong-key row below encrypts under the first key and decrypts
    // under the second, and asserts a PADDING failure. Both moduli are
    // 2048-bit and otherwise unrelated, so roughly half the time the second
    // is the smaller of the two — and then the ciphertext integer (uniform
    // below the FIRST modulus) is sometimes at or above the second, where
    // `RSACore.parseMsg`'s guard rejects it as
    // `BadPaddingException("Message is larger than modulus")` BEFORE any
    // unpadding runs. Same class, different message, and the message is
    // what the VULN(1) oracle repair pins:
    //
    // ```text
    // assertion `left == right` failed: Pkcs1
    //   left: "Message is larger than modulus"
    //  right: "Padding error in decryption"
    // ```
    //
    // Sorting the pair so the ENCRYPTING modulus is the smaller one makes
    // `ct < n <= other_n` hold by construction, so the ciphertext is always
    // a decryptable representative under the wrong key and the failure is
    // always the unpadding one the test is about. Relaxing the assertion
    // instead would retire the oracle regression it exists for.
    let (a_pk, a_sk) = Rsa::generate_keypair(2048);
    let (b_pk, b_sk) = Rsa::generate_keypair(2048);
    let ((pk, sk), (other_pk, other_sk)) = if a_pk.n.cmp(&b_pk.n) == std::cmp::Ordering::Less {
        ((a_pk, a_sk), (b_pk, b_sk))
    } else {
        ((b_pk, b_sk), (a_pk, a_sk))
    };
    let n = pk.n.to_bytes_be();
    let e = pk.e.to_bytes_be();
    let d = sk.d.to_bytes_be();
    let other_n = other_pk.n.to_bytes_be();
    let other_d = other_sk.d.to_bytes_be();

    let bad = "javax/crypto/BadPaddingException";
    let size = "javax/crypto/IllegalBlockSizeException";

    for pad in [
        RsaCipherPadding::Pkcs1,
        RsaCipherPadding::OaepSha1,
        RsaCipherPadding::OaepSha256,
    ] {
        // Re-roll until the ciphertext's leading byte is non-zero.
        // `rsa_cipher_encrypt` emits exactly `k` bytes via
        // `to_bytes_be_padded`, so a ciphertext below 2^(8(k-1)) — about 1
        // in 128 for this modulus — carries a leading zero, and the
        // "shorter than the modulus" row below then hands `decrypt` the
        // SAME integer, which decrypts correctly and fails an assertion
        // that is about length, not about padding. Measured at 3/60 runs
        // before this loop, on both this tree and dev. Encryption is
        // randomized (PKCS#1 type 2 / OAEP seed), so re-rolling is free and
        // terminates immediately.
        let ct = loop {
            let c = rsa_cipher_encrypt(&n, &e, pad, b"payload").expect("encrypt");
            if c[0] != 0 {
                break c;
            }
        };

        // The WRONG PRIVATE KEY and a CORRUPTED CIPHERTEXT are padding
        // failures. This is the row the regression suite sampled.
        let wrong_key = rsa_cipher_decrypt(&other_n, &other_d, pad, &ct)
            .expect_err("the wrong private key must not decrypt");
        assert_eq!(wrong_key.jca_class(), bad, "{pad:?}: wrong key");
        let mut corrupt = ct.clone();
        corrupt[200] ^= 0x01;
        let flipped = rsa_cipher_decrypt(&n, &d, pad, &corrupt)
            .expect_err("a flipped ciphertext byte must not decrypt");
        assert_eq!(flipped.jca_class(), bad, "{pad:?}: corrupted ciphertext");

        // Every padding failure must ALSO still carry the single opaque
        // message the VULN(1) constant-time repair collapsed them to.
        // Widening the exception surface must not reopen the
        // Bleichenbacher/Manger oracle.
        assert_eq!(wrong_key.message(), RSA_PADDING_ERROR, "{pad:?}");
        assert_eq!(flipped.message(), RSA_PADDING_ERROR, "{pad:?}");

        // A SHORTER-than-modulus ciphertext is a smaller integer that
        // decrypts and fails to unpad, NOT a block-size failure. A LONGER
        // one is the reverse. The asymmetry is SunJCE's, and a
        // `ct.len() != k` check gets it wrong in one direction while
        // getting the class wrong in both.
        let short = rsa_cipher_decrypt(&n, &d, pad, &ct[1..])
            .expect_err("a short ciphertext must not decrypt");
        assert_eq!(short.jca_class(), bad, "{pad:?}: short ciphertext");
        let mut long = ct.clone();
        long.push(0);
        let long_err = rsa_cipher_decrypt(&n, &d, pad, &long)
            .expect_err("an over-long ciphertext must be refused");
        assert_eq!(long_err.jca_class(), size, "{pad:?}: over-long ciphertext");
        assert_eq!(
            long_err.message(),
            "Data must not be longer than 256 bytes",
            "{pad:?}: SunJCE's own wording"
        );

        // Too much plaintext for the padding: 245 for PKCS#1 v1.5, 214 for
        // OAEP-SHA-1, 190 for OAEP-SHA-256 under a 2048-bit modulus.
        let max = pad.max_data_size(256).expect("2048-bit modulus fits");
        assert!(
            rsa_cipher_encrypt(&n, &e, pad, &vec![0u8; max]).is_ok(),
            "{pad:?}: exactly max_data_size must still encrypt"
        );
        let too_long = rsa_cipher_encrypt(&n, &e, pad, &vec![0u8; max + 1])
            .expect_err("one byte over the padding limit must be refused");
        assert_eq!(too_long.jca_class(), size, "{pad:?}: plaintext too long");
        assert_eq!(
            too_long.message(),
            format!("Data must not be longer than {max} bytes"),
            "{pad:?}: SunJCE's own wording"
        );
    }

    // The measured limits, so a change to `hlen` or to the overheads shows
    // up here as a number rather than as a silently different refusal.
    assert_eq!(RsaCipherPadding::Pkcs1.max_data_size(256), Some(245));
    assert_eq!(RsaCipherPadding::OaepSha1.max_data_size(256), Some(214));
    assert_eq!(RsaCipherPadding::OaepSha256.max_data_size(256), Some(190));
    // A modulus too small to hold the padding is a KEY problem, not a data
    // one — SunJCE raises InvalidKeyException from `RSAPadding.getInstance`.
    assert_eq!(RsaCipherPadding::OaepSha256.max_data_size(66), None);
    assert_eq!(RsaCipherPadding::Pkcs1.max_data_size(11), None);

    // ANTI-VACUITY: the three variants must map to three DIFFERENT classes.
    // A `jca_class` that answered `BadPaddingException` for everything
    // would satisfy most of the assertions above and would be the same
    // defect one level down.
    assert_ne!(
        RsaCipherError::BlockSize(String::new()).jca_class(),
        RsaCipherError::Padding(String::new()).jca_class()
    );
    assert_ne!(
        RsaCipherError::Key(String::new()).jca_class(),
        RsaCipherError::Padding(String::new()).jca_class()
    );
    // And none of them may be an unchecked class — the whole point.
    for e in [
        RsaCipherError::BlockSize(String::new()),
        RsaCipherError::Padding(String::new()),
        RsaCipherError::Key(String::new()),
    ] {
        assert!(
            !e.jca_class().starts_with("java/lang/"),
            "{:?} must not be a java.lang (unchecked) exception",
            e
        );
    }
}

#[test]
fn rsa_cipher_padding_from_transformation() {
    assert_eq!(
        RsaCipherPadding::from_transformation("PKCS1Padding"),
        Some(RsaCipherPadding::Pkcs1)
    );
    assert_eq!(
        RsaCipherPadding::from_transformation("OAEPWithSHA-1AndMGF1Padding"),
        Some(RsaCipherPadding::OaepSha1)
    );
    assert_eq!(
        RsaCipherPadding::from_transformation("OAEPWithSHA-256AndMGF1Padding"),
        Some(RsaCipherPadding::OaepSha256)
    );
    assert_eq!(RsaCipherPadding::from_transformation("NoPadding"), None);
}

#[test]
fn rsa_pss_verify_hotspot_vectors() {
    // PS256/PS384/PS512 signatures produced by HotSpot's RSASSA-PSS SPI
    // (salt length == hash length, MGF1 over the same hash) over a fixed
    // 2048-bit key — validates EMSA-PSS-VERIFY against a real implementation.
    let n = from_hex("00e2887dc7a26dee90a6811cb259ee83a027a132771e3811a33768a6ef96a1090e793c4d042bfd04f52e8ae497a40a1b71a6acd8d451f35c7f6804d3c46a30e1f00d03b8542397f87aec655447c33f11998a07bf505dfc1c623148cbc6e2a1a9d88f44ed77ecb5813ae51c2db6043077223da796509e4158f5fa2f97cbebd28ad78dc9a7c5c48ed6131ee3cd897605ed771c7cd55d91dfb14eddc27164840803d67c9cb0ec3d7077d91921a3ea0b44c791b1b06fa1de4ea39cadca9a982704b30b3f07e35d1edd1c56d11907b44e46986b9f2edfce56111a4ab21d441bd884c7442aed05b99502bbc0171ba74ca08abac0dda1376ddcda9dca78124f7ee2284619");
    let e = from_hex("010001");
    let msg = from_hex("74686520717569636b2062726f776e20666f78206a756d7073206f76657220746865206c617a7920646f67");
    for (h, sig_hex) in [
        (PssHash::Sha256, "e12b6161e1f1912f87d64e4face0199d9b95b74bac37e2e257dd129e56fc618094031beb07f888577af2749ed933d171111640f824dad6b55fb3175304d6ee44420dcef4e6a0be6c5ae1f750aa8352d7302ac75bb3f6201fe73d5ba667427c467d0e3a93ea9eac799a105f23f6dd04e0bcc240517ef3fb58cfb2ad3fd6d6bd37ca917dfc5b1e72aa42defc90780434745f36e9cb11f7b932a063ea3d821a8fd008580c0103ca35df29409539bd62d1c7cdb42e26f857f5c9abe65f3057306bae225918d13b1780fc87fbe35b338b27b40619a7928edce8b17427b5232315538e755f6bfb051210f8f2af14d0bebe39914be2f44c0c992a4b32e4acaf9689ef95"),
        (PssHash::Sha384, "85a0d54722469e406e11391c33c0c63a5c931bc05c85f5e0e91c0f689dfdccf983e472eceae07e76f46b3643ae55a6b21e5595dfaefd2581d3f802146f785af290dbd08300a872a77883e16dbba5c6f2c43413ed9d6b6ba8406aed6c698f839acec64c6917d833f107b248afd1851074f80b535ac3c8f52dd4251d203d4b332e34d610ca86534305ea936798d1ca4313c1226f10da5f25bf3d99d2b7cb63e6d63990e3713284a3a746155ea8b1bd11c0fa44408aecf9bebb002b3c18d6cbe3fb03562c3d12d9c83127af0b2b48f1336f37dac7e8c904e83b897a2614eef1ba329cbca5183e661622b8f067dd9696568b5b24f9c38e56b704b6dd7d33c1c39bf7"),
        (PssHash::Sha512, "1aa9ecf5d993ccd238f23b07f08ab30af2298bbe1b1a26d85efdef2550a02429b58d14a8a4d6a4f27e9fbfa36f51ddb46bbd5b1fdc3921f2031b0aecc3ea245f76e94a482fe232db7a5b1aa8515e24165bab95dd3fa4ec03d55e98a95201f7f858865d8dc96bc46b863db5f9c1e11d07d238faef3dd419c4fe970d07c7e1c5d8d252b956198e9358e6b37a50417a69cd5353a3dc74765568038769fa32ac0758b2161aa1399ef1c4101a465dfa517f70e1d23d158359fa52e0f6ddc4b3f54ece116f24d6478701e6ebf7e2dcf19e97af6b0542237ddcac5b81cce62b6467502de9421a469f4f43ba048e05589860b3b7e4a9004a7b99ca557e2e85ba2a871f7f"),
    ] {
        let sig = from_hex(sig_hex);
        assert!(rsa_verify_pss(&n, &e, h, &msg, &sig), "{:?}: valid PSS sig", h);
        let mut bad = msg.clone();
        bad[0] ^= 1;
        assert!(!rsa_verify_pss(&n, &e, h, &bad, &sig), "{:?}: tampered msg", h);
    }
}

#[test]
fn rsa_pss_sign_verify_round_trip() {
    let (public, private) = Rsa::generate_keypair(1024);
    let message = b"CratonVM RSASSA-PSS round trip";
    let signature = rsa_sign_pss(&private, PssHash::Sha256, message);
    assert_eq!(signature.len(), 128);
    assert!(rsa_verify_pss(
        &public.n.to_bytes_be(),
        &public.e.to_bytes_be(),
        PssHash::Sha256,
        message,
        &signature,
    ));
}

// RF.10: PKIX chain walker — helper unit coverage.

// Build a minimal X509Cert with the fields `verify_cert_chain` reads.
// We skip signature bytes (use an always-valid mock algorithm) by
// routing subject/issuer through cert stubs and then exercising the
// branches we own.
fn mock_cert(subject: &str, issuer: &str, not_before: i64, not_after: i64) -> X509Cert {
    X509Cert {
        version: 3,
        serial_number: vec![0x01],
        sig_algorithm: "Unknown".to_string(), // forces verify_signature → false
        issuer_raw: issuer.as_bytes().to_vec(),
        issuer_cn: issuer.to_string(),
        subject_raw: subject.as_bytes().to_vec(),
        subject_cn: subject.to_string(),
        not_before,
        not_after,
        public_key_bytes: vec![],
        public_key_algorithm: "RSA".to_string(),
        signature_bytes: vec![],
        tbs_bytes: vec![],
        encoded: vec![],
    }
}

#[test]
fn rf10_empty_chain_rejected() {
    let err = verify_cert_chain(&[], &[], 0).unwrap_err();
    assert_eq!(err, PkixError::EmptyChain);
}

#[test]
fn rf10_chain_too_long_rejected() {
    // Use 11 mock certs — the limit is 10.
    let chain: Vec<X509Cert> = (0..11)
        .map(|i| mock_cert(&format!("c{i}"), &format!("c{}", i + 1), 0, i64::MAX))
        .collect();
    let err = verify_cert_chain(&chain, &[], 0).unwrap_err();
    assert_eq!(err, PkixError::ChainTooLong);
}

#[test]
fn rf10_untrusted_root_rejected() {
    let leaf = mock_cert("leaf", "intermediate", 0, i64::MAX);
    // No anchors provided — validation must fail.
    let err = verify_cert_chain(&[leaf], &[], 0).unwrap_err();
    match err {
        PkixError::UntrustedRoot { subject } => assert_eq!(subject, "leaf"),
        other => panic!("expected UntrustedRoot, got {other:?}"),
    }
}

#[test]
fn rf10_expired_cert_rejected() {
    // not_before in the past, not_after also in the past → expired.
    let expired = mock_cert("expired", "ca", 1_000, 2_000);
    let err = verify_cert_chain(&[expired], &[], 10_000).unwrap_err();
    match err {
        PkixError::Expired { subject } => assert_eq!(subject, "expired"),
        other => panic!("expected Expired, got {other:?}"),
    }
}

#[test]
fn rf10_pkix_error_display_messages() {
    assert_eq!(
        format!("{}", PkixError::EmptyChain),
        "empty certificate chain"
    );
    assert!(format!(
        "{}",
        PkixError::Expired {
            subject: "x".into()
        }
    )
    .contains("expired"));
    assert!(format!(
        "{}",
        PkixError::UntrustedRoot {
            subject: "y".into()
        }
    )
    .contains("trust anchor"));
    assert_eq!(
        format!("{}", PkixError::ChainTooLong),
        "chain exceeds maximum depth"
    );
}

#[test]
fn rf10_subject_cn_matches_basic() {
    let c = mock_cert("root", "root", 0, i64::MAX);
    assert!(c.subject_cn_matches("root"));
    assert!(!c.subject_cn_matches("other"));

    let empty = mock_cert("", "", 0, i64::MAX);
    // An empty subject must not match anything — otherwise an unparsed
    // certificate would pretend to be a trust anchor.
    assert!(!empty.subject_cn_matches(""));
}

// RF.7: OS CSPRNG produces non-repeating output. `os_random_bytes` is
// already exercised by `os_random_bytes_returns_distinct_bytes` above;
// here we confirm that back-to-back draws produce independent streams.
#[test]
fn rf7_os_random_back_to_back_differs() {
    let mut a = [0u8; 64];
    let mut b = [0u8; 64];
    assert!(os_random_bytes(&mut a));
    assert!(os_random_bytes(&mut b));
    // Two 64-byte draws from a CSPRNG should effectively never match.
    assert_ne!(a, b, "consecutive CSPRNG draws must differ");
}

// B1: a tiny RSA modulus (256-bit → k=32, below tLen+11=62 for SHA-256)
// must be rejected as a verification failure, NOT underflow the PKCS#1
// v1.5 padding-length math (panic in debug / exabyte alloc in release).
// Reachable from `verify_signature`/`checkServerTrusted` with an
// attacker-supplied issuer key.
#[test]
fn b1_small_rsa_modulus_verify_rejects_without_panic() {
    let n = BigUint::from_bytes_be(&[0xFFu8; 32]); // 256-bit modulus
    let e = BigUint::from_bytes_be(&[0x01, 0x00, 0x01]);
    let key = RsaPublicKey { n, e };
    // Signature length must equal k (=32) to reach the encode step.
    let sig = vec![0x01u8; 32];
    assert!(
        !Rsa::verify_sha256(&key, b"anything", &sig),
        "small-modulus RSA verify must fail closed, not panic"
    );
}

// B1: signing with a too-small key returns an empty signature (treated as
// a failure by callers) instead of underflowing the padding math.
#[test]
fn b1_small_rsa_modulus_sign_returns_empty() {
    let n = BigUint::from_bytes_be(&[0xFFu8; 32]);
    let d = BigUint::from_bytes_be(&[0x03]);
    let e = BigUint::from_bytes_be(&[0x01, 0x00, 0x01]);
    let key = RsaPrivateKey {
        n,
        d,
        e,
        p: None,
        q: None,
        dp: None,
        dq: None,
        qinv: None,
    };
    assert!(
        Rsa::sign_sha256(&key, b"anything").is_empty(),
        "sign with a sub-minimum modulus must yield an empty signature"
    );
}

// B2: a DER length field encoded in 8 bytes near usize::MAX must be
// rejected (None), not wrap `total_hdr + len` and produce an inverted /
// out-of-range slice panic.
#[test]
fn b2_der_oversized_length_rejected_without_panic() {
    // tag=0x30, long-form length: 0x88 (=> 8 length octets) all 0xFF,
    // then a couple of content bytes. The encoded length is ~usize::MAX.
    let mut data = vec![0x30u8, 0x88];
    data.extend_from_slice(&[0xFFu8; 8]);
    data.extend_from_slice(&[0x01, 0x02]);
    assert!(
        der_read_tag_length(&data).is_none(),
        "oversized DER length must be rejected, not panic"
    );
}

// B2: a length wider than a usize (9 length octets) is rejected outright.
#[test]
fn b2_der_overwide_length_field_rejected() {
    let mut data = vec![0x30u8, 0x89]; // 9 length octets — too wide
    data.extend_from_slice(&[0xFFu8; 9]);
    data.push(0x00);
    assert!(der_read_tag_length(&data).is_none());
}

// B2: a parseable but truncated certificate (length exceeds buffer) is a
// clean parse error, not a panic.
#[test]
fn b2_truncated_der_is_parse_error() {
    // SEQUENCE claiming 100 content bytes but only 3 present.
    let data = vec![0x30u8, 100, 0x01, 0x02, 0x03];
    assert!(der_read_tag_length(&data).is_none());
}

// V1: full Name-DER matching — a cert whose subject DER differs must not
// match even if a CN string would have collided.
#[test]
fn v1_subject_der_matches_uses_full_name() {
    let c = mock_cert("root", "root", 0, i64::MAX);
    // mock_cert sets subject_raw = subject.as_bytes().
    assert!(c.subject_der_matches(b"root"));
    assert!(!c.subject_der_matches(b"r00t"));
    // Empty subject Name never matches (an unparsed cert can't be anchor).
    let empty = mock_cert("", "", 0, i64::MAX);
    assert!(!empty.subject_der_matches(b""));
}

// -----------------------------------------------------------------------
// nb-crypto-impl VULN(1) — RSA padding-oracle regression tests.
//
// Every PKCS#1 v1.5 and OAEP padding failure must collapse to the SAME
// opaque error string (`RSA_PADDING_ERROR`); none may leak which structural
// check failed. Valid padding must still round-trip.
// -----------------------------------------------------------------------

#[test]
fn pkcs1_unpad_roundtrip_ok() {
    let k = 128; // RSA-1024 modulus byte length
    let msg = b"hello pkcs1";
    let em = rsa_pkcs1_type2_pad(msg, k).unwrap();
    assert_eq!(em.len(), k);
    assert_eq!(rsa_pkcs1_type2_unpad(&em).unwrap(), msg.to_vec());
}

#[test]
fn pkcs1_unpad_all_failures_indistinguishable() {
    let k = 128;
    let good = rsa_pkcs1_type2_pad(b"x", k).unwrap();

    // Wrong leading byte.
    let mut a = good.clone();
    a[0] = 0x01;
    // Wrong block type.
    let mut b = good.clone();
    b[1] = 0x01;
    // No 0x00 separator anywhere after the type byte.
    let mut c = good.clone();
    for byte in c.iter_mut().skip(2) {
        *byte = 0xFF;
    }
    // Separator too early -> PS shorter than 8 bytes.
    let mut d = vec![0x00u8, 0x02, 0x01, 0x02, 0x00];
    d.extend(std::iter::repeat(0xAAu8).take(k - d.len()));
    // Too short to hold any valid padding.
    let e = vec![0x00u8, 0x02, 0x00];

    for bad in [&a, &b, &c, &d, &e] {
        let err = rsa_pkcs1_type2_unpad(bad).unwrap_err();
        assert_eq!(err, RSA_PADDING_ERROR, "PKCS1 error string leaked detail");
    }
}

#[test]
fn oaep_unpad_roundtrip_ok() {
    let k = 256; // RSA-2048
    for pad in [RsaCipherPadding::OaepSha1, RsaCipherPadding::OaepSha256] {
        let msg = b"oaep payload";
        let em = rsa_oaep_pad(pad, msg, k).unwrap();
        assert_eq!(em.len(), k);
        assert_eq!(rsa_oaep_unpad(pad, &em).unwrap(), msg.to_vec());
    }
}

#[test]
fn oaep_unpad_all_failures_indistinguishable() {
    let k = 256;
    let pad = RsaCipherPadding::OaepSha256;
    let good = rsa_oaep_pad(pad, b"y", k).unwrap();

    // Corrupt leading Y byte (must be 0x00) -> Manger oracle bait.
    let mut a = good.clone();
    a[0] ^= 0x01;
    // Corrupt the masked DB so the recovered lHash mismatches.
    let mut b = good.clone();
    let last = b.len() - 1;
    b[last] ^= 0xFF;
    // Corrupt the masked seed region (also perturbs DB unmask -> mismatch).
    let mut c = good.clone();
    c[1] ^= 0xFF;
    // Too short to hold `00 || seed || DB`.
    let d = vec![0u8; 2 * pad.hlen() + 1];

    for bad in [&a, &b, &c, &d] {
        let err = rsa_oaep_unpad(pad, bad).unwrap_err();
        assert_eq!(err, RSA_PADDING_ERROR, "OAEP error string leaked detail");
    }
}

#[test]
fn ct_helpers_behave() {
    assert_eq!(ct_eq_u8(0x42, 0x42), 0xFF);
    assert_eq!(ct_eq_u8(0x42, 0x43), 0x00);
    assert_eq!(ct_is_nonzero_u8(0), 0x00);
    assert_eq!(ct_is_nonzero_u8(1), 0xFF);
    assert_eq!(ct_is_nonzero_u8(0xFF), 0xFF);
    assert_eq!(ct_eq_bytes(b"abc", b"abc"), 0xFF);
    assert_eq!(ct_eq_bytes(b"abc", b"abd"), 0x00);
    assert_eq!(ct_eq_bytes(b"abc", b"ab"), 0x00);
}

// -----------------------------------------------------------------------
// Migration off the ambiguous `bool` wrapper.
//
// `Rsa::verify_sha256` was the last call site of
// `native-builtins-crypto`'s `verify_rsa_pkcs1_v15` (the `-> bool` form
// the crypto-failure contract flags as ambiguous). `rsa_verify` — the
// backend behind `Signature.verify()` — now goes through the *checked*
// form so that "the backend refused this key" and "the signature does not
// match" are two different answers.
// -----------------------------------------------------------------------

#[test]
fn rsa_verify_separates_a_refused_key_from_a_failed_verification() {
    // MUST STILL WORK: a real key round-trips, and a tampered signature is
    // a plain `Some(false)` — the genuine negative, preserved.
    let (public_key, private_key) = Rsa::generate_keypair(1024);
    let good_n = public_key.n.clone();
    let id = rsa_key_next_id();
    rsa_key_store(
        id,
        RsaKeyPairData {
            public_key,
            private_key,
        },
    );
    let msg = b"a message worth signing";
    let sig = rsa_sign(id, msg).expect("a registered key must sign");
    assert_eq!(rsa_verify(id, msg, &sig), Some(true));

    let mut tampered = sig.clone();
    tampered[0] ^= 0xff;
    assert_eq!(
        rsa_verify(id, msg, &tampered),
        Some(false),
        "a real digest mismatch is the answer to the question asked"
    );
    assert_eq!(rsa_verify(id, b"a different message", &sig), Some(false));

    // MUST BE UNANSWERABLE: an even public exponent is rejected by the
    // backend before any RSA operation runs. Collapsed to `false` before
    // this migration, which the caller could not tell from a forgery.
    let (_ignored_pub, refused_priv) = Rsa::generate_keypair(1024);
    let refused_id = rsa_key_next_id();
    rsa_key_store(
        refused_id,
        RsaKeyPairData {
            public_key: RsaPublicKey {
                n: good_n,
                e: BigUint::from_u64(4), // even — not a valid RSA exponent
            },
            private_key: refused_priv,
        },
    );
    assert_eq!(
        rsa_verify(refused_id, msg, &sig),
        None,
        "a key the backend refuses must not answer the verification question"
    );

    // And an unregistered handle stays `None`, as before.
    assert_eq!(rsa_verify(u64::MAX, msg, &sig), None);
}

/// The `bool` surface that certificate-chain validation still uses must
/// stay fail-closed: neither a refusal nor a mismatch may become `true`.
#[test]
fn the_bool_verify_surface_is_still_fail_closed() {
    let (public_key, private_key) = Rsa::generate_keypair(1024);
    let msg = b"chain-validation payload";
    let sig = Rsa::sign_sha256(&private_key, msg);
    assert!(Rsa::verify_sha256(&public_key, msg, &sig));

    let mut tampered = sig.clone();
    tampered[0] ^= 0xff;
    assert!(!Rsa::verify_sha256(&public_key, msg, &tampered));

    let refused = RsaPublicKey {
        n: public_key.n.clone(),
        e: BigUint::from_u64(4),
    };
    assert!(
        !Rsa::verify_sha256(&refused, msg, &sig),
        "an Err must never surface as true"
    );
    assert!(Rsa::try_verify_sha256(&refused, msg, &sig).is_err());
}
