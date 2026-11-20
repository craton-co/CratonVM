use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// T2.6.19 — SHA-256("hello") matches RFC 6234 test vector
/// `2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824`
#[test]
fn t2_6_19_sha256_hello_matches_rfc6234() {
    let digest = compute_digest("SHA-256", b"hello").unwrap();
    let expected = [
        0x2c, 0xf2, 0x4d, 0xba, 0x5f, 0xb0, 0xa3, 0x0e, 0x26, 0xe8, 0x3b, 0x2a, 0xc5, 0xb9,
        0xe2, 0x9e, 0x1b, 0x16, 0x1e, 0x5c, 0x1f, 0xa7, 0x42, 0x5e, 0x73, 0x04, 0x33, 0x62,
        0x93, 0x8b, 0x98, 0x24,
    ];
    assert_eq!(
        digest, expected,
        "SHA-256(\"hello\") did not match RFC 6234 vector"
    );
}

/// MD2 (RFC 1319) against HotSpot 25's own `MessageDigest.getInstance("MD2")`.
///
/// This lane could not build or run Rust, so `real_md2` was adjudicated
/// BEFORE it was written: the identical transcription was expressed in
/// Java and run against jdk-25.0.3.9-hotspot on these exact ten messages,
/// all ten matching. This test is what carries that verification into the
/// tree — landing a hand-written digest without published vectors beside
/// it would be the same class of mistake as the SHA-256 fallback it
/// replaces. W7-63-jca-advertise-vs-serve.md.
///
/// The three-way length coverage is deliberate. MD2's padding rule adds
/// between 1 and 16 bytes and NEVER zero, so a message that is already an
/// exact multiple of 16 takes a full extra block of `0x10`. That is the
/// boundary a naive implementation gets wrong and the reason the 15/16/17
/// rows are here rather than a single vector.
#[test]
fn real_md2_matches_hotspot_vectors() {
    let hex = |h: Vec<u8>| -> String { h.iter().map(|b| format!("{b:02x}")).collect() };
    for (msg, want) in [
        ("", "8350e5a3e24c153df2275c9f80692773"),
        ("a", "32ec01ec4a6dac72c0ab96fb34c0b5d1"),
        ("abc", "da853b0d3f88d99b30283a69e6ded6bb"),
        ("message digest", "ab4f496bfb2a530b219ff33031fe06b0"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "4e8ddff3650292ab5a4108c3aa47940b",
        ),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "da33def2a42df13975352846c30338cd",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "d5976f79d83d3a0dc9806c3c66f3efd8",
        ),
        // 15 bytes: a partial pad.
        ("0123456789abcde", "d95629645108a20ab4d70e8545e0723b"),
        // 16 bytes: an EXACT multiple, so a whole extra 0x10 pad block.
        ("0123456789abcdef", "12c8dfa285f14e1af8c5254e7092d0d3"),
        // 17 bytes: two blocks plus a 15-byte pad.
        ("0123456789abcdefg", "e4d0efded5ef7b6843a5ba47e1171347"),
    ] {
        assert_eq!(hex(real_md2(msg.as_bytes())), want, "MD2({msg:?})");
        // And through the dispatcher, which is the surface `MessageDigest`
        // actually reaches — a correct `real_md2` wired to the wrong arm
        // name would pass the line above and fail this one.
        assert_eq!(
            hex(compute_digest("MD2", msg.as_bytes()).unwrap()),
            want,
            "compute_digest(\"MD2\", {msg:?})"
        );
    }
}

/// T2.6.2 companion — SHA3-256("") matches the FIPS 202 empty-string
/// test vector `a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a`.
#[test]
fn t2_6_2_sha3_256_empty_matches_fips202() {
    let digest = compute_digest("SHA3-256", b"").unwrap();
    let expected = [
        0xa7, 0xff, 0xc6, 0xf8, 0xbf, 0x1e, 0xd7, 0x66, 0x51, 0xc1, 0x47, 0x56, 0xa0, 0x61,
        0xd6, 0x62, 0xf5, 0x80, 0xff, 0x4d, 0xe4, 0x3b, 0x49, 0xfa, 0x82, 0xd8, 0x0a, 0x4b,
        0x80, 0xf8, 0x43, 0x4a,
    ];
    assert_eq!(digest, expected, "SHA3-256(\"\") did not match FIPS 202");
}

/// T2.6.2 companion — SHA3-512("abc") matches the FIPS 202 vector.
#[test]
fn t2_6_2_sha3_512_abc_matches_fips202() {
    let digest = compute_digest("SHA3-512", b"abc").unwrap();
    assert_eq!(digest.len(), 64);
    // First 8 bytes suffice to catch catastrophic backend swaps.
    let expected_prefix = [0xb7, 0x51, 0x85, 0x0b, 0x1a, 0x57, 0x16, 0x8a];
    assert_eq!(&digest[..8], &expected_prefix);
}

/// T2.6.5 — ChaCha20-Poly1305 round-trip via the RustCrypto backend.
/// This does not exercise the JVM dispatch path; it verifies the
/// encrypt/decrypt invariant that guards the `is_chacha_poly` branch
/// in `cipher_do_final`.
#[test]
fn t2_6_5_chacha20_poly1305_round_trip() {
    use chacha20poly1305::{
        aead::{Aead, KeyInit, Payload},
        ChaCha20Poly1305, Key, Nonce,
    };
    let key = [0x11u8; 32];
    let nonce_bytes = [0x22u8; 12];
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = b"T2.6.5 ChaCha20-Poly1305 acceptance".to_vec();
    let aad = b"cratonvm-t2.6".to_vec();

    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: &plaintext,
                aad: &aad,
            },
        )
        .expect("encrypt");
    assert!(ct.len() > plaintext.len(), "ciphertext must include tag");

    let round_trip = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &ct,
                aad: &aad,
            },
        )
        .expect("decrypt");
    assert_eq!(round_trip, plaintext);

    // Tamper with the AAD — decryption must fail, proving the tag
    // actually authenticates associated data.
    let tampered = cipher.decrypt(
        nonce,
        Payload {
            msg: &ct,
            aad: b"evil",
        },
    );
    assert!(
        tampered.is_err(),
        "AAD substitution must fail authentication"
    );
}

/// T2.6.5 — AES-CTR round-trip (encrypt = decrypt) at all three key
/// lengths, via the exact same RustCrypto types wired into the
/// Cipher backend.
#[test]
fn t2_6_5_aes_ctr_round_trip_128_192_256() {
    use aes::cipher::{KeyIvInit, StreamCipher};
    type C128 = ctr::Ctr64BE<aes::Aes128>;
    type C192 = ctr::Ctr64BE<aes::Aes192>;
    type C256 = ctr::Ctr64BE<aes::Aes256>;
    let iv = [0x33u8; 16];
    let msg = b"T2.6.5 AES-CTR acceptance payload".to_vec();

    for keylen in [16usize, 24, 32] {
        let key: Vec<u8> = (0..keylen).map(|i| i as u8 ^ 0x5a).collect();
        let mut buf = msg.clone();
        match keylen {
            16 => {
                let mut c = C128::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            24 => {
                let mut c = C192::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            32 => {
                let mut c = C256::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            _ => unreachable!(),
        }
        assert_ne!(buf, msg, "ciphertext must differ from plaintext");

        // CTR is its own inverse with the same (key, iv).
        match keylen {
            16 => {
                let mut c = C128::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            24 => {
                let mut c = C192::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            32 => {
                let mut c = C256::new(key.as_slice().into(), (&iv).into());
                c.apply_keystream(&mut buf);
            }
            _ => unreachable!(),
        }
        assert_eq!(buf, msg, "AES-{}-CTR round-trip mismatch", keylen * 8);        }    }