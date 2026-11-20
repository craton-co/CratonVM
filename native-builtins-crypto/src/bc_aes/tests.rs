use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}
fn arr16(s: &str) -> [u8; 16] {
    let mut b = [0u8; 16];
    b.copy_from_slice(&unhex(s));
    b
}

// Validate the full chain — generate_working_key + encrypt/decrypt_block —
// against FIPS-197 known-answer vectors.
fn kat(key_hex: &str, pt_hex: &str, ct_hex: &str) {
    let key = unhex(key_hex);
    let pt = arr16(pt_hex);
    let expect_ct = arr16(ct_hex);

    let enc = generate_working_key(&key, true).unwrap();
    let mut ct = [0u8; 16];
    encrypt_block(&enc, &pt, &mut ct);
    assert_eq!(ct, expect_ct, "encrypt KAT key={key_hex}");

    let dec = generate_working_key(&key, false).unwrap();
    let mut back = [0u8; 16];
    decrypt_block(&dec, &ct, &mut back);
    assert_eq!(back, pt, "decrypt round-trip key={key_hex}");
}

#[test]
fn fips197_aes128() {
    // FIPS-197 Appendix C.1 and Appendix B
    kat(
        "000102030405060708090a0b0c0d0e0f",
        "00112233445566778899aabbccddeeff",
        "69c4e0d86a7b0430d8cdb78070b4c55a",
    );
    kat(
        "2b7e151628aed2a6abf7158809cf4f3c",
        "3243f6a8885a308d313198a2e0370734",
        "3925841d02dc09fbdc118597196a0b32",
    );
}

#[test]
fn fips197_aes192_aes256() {
    // FIPS-197 Appendix C.2 (AES-192) and C.3 (AES-256)
    kat(
        "000102030405060708090a0b0c0d0e0f1011121314151617",
        "00112233445566778899aabbccddeeff",
        "dda97ca4864cdfe06eaf70a0ec0d7191",
    );
    kat(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        "00112233445566778899aabbccddeeff",
        "8ea2b7ca516745bfeafc49904b496089",
    );
}

#[test]
fn invalid_key_length_is_none() {
    assert!(generate_working_key(&[0u8; 15], true).is_none());
    assert!(generate_working_key(&[0u8; 20], true).is_none());
    assert!(generate_working_key(&[0u8; 33], true).is_none());
}

// ---- fail-loud guards ----

/// Exactly the three FIPS-197 key sizes are accepted; every other length
/// raises rather than producing a schedule from truncated/padded material.
#[test]
fn supported_key_lengths_succeed_and_others_raise() {
    for len in [16usize, 24, 32] {
        let k = vec![0u8; len];
        assert!(
            try_generate_working_key(&k, true).is_ok(),
            "{len}-byte key should be supported"
        );
        assert!(try_generate_working_key(&k, false).is_ok());
    }
    for len in [0usize, 1, 8, 15, 17, 20, 23, 25, 31, 33, 64] {
        let err = try_generate_working_key(&vec![0u8; len], true)
            .expect_err("unsupported key length must be rejected");
        assert_eq!(
            err.java_class(),
            "java/lang/IllegalArgumentException",
            "{len}-byte key: wrong exception type"
        );
        assert!(
            err.message().contains("128/192/256"),
            "{len}-byte key: message should name the supported sizes"
        );
    }
}

/// An unkeyed or malformed engine must raise, not encrypt. Before the guard
/// an empty schedule underflowed `kw.len() - 1`, and a *non-empty* but
/// wrong-length one (2 or 4 entries) still ran off the end of `kw` on the
/// final round — so a lower bound alone would not have been enough.
#[test]
fn malformed_key_schedule_raises_illegal_state() {
    let inb = [0u8; 16];
    let mut outb = [0u8; 16];
    for len in [0usize, 1, 2, 3, 4, 10, 12, 14, 16] {
        let kw = vec![[0u32; 4]; len];
        let err = try_encrypt_block(&kw, &inb, &mut outb)
            .expect_err("malformed schedule must be rejected");
        assert_eq!(
            err.java_class(),
            "java/lang/IllegalStateException",
            "{len}-entry schedule: wrong exception type"
        );
        let err = try_decrypt_block(&kw, &inb, &mut outb)
            .expect_err("malformed schedule must be rejected");
        assert_eq!(err.java_class(), "java/lang/IllegalStateException");
    }
    // ...and the output buffer was left untouched — no zero "ciphertext"
    // was produced that a caller could mistake for a real transform.
    assert_eq!(outb, [0u8; 16]);
}

/// The three real AES schedule lengths are accepted, so the strict check
/// cannot reject a legitimately keyed engine.
#[test]
fn every_real_key_size_produces_an_accepted_schedule() {
    let inb = [0u8; 16];
    let mut outb = [0u8; 16];
    for key_len in [16usize, 24, 32] {
        let kw = generate_working_key(&vec![0u8; key_len], true).unwrap();
        assert!(
            VALID_SCHEDULE_LENS.contains(&kw.len()),
            "{key_len}-byte key produced a {}-entry schedule",
            kw.len()
        );
        try_encrypt_block(&kw, &inb, &mut outb).expect("real schedule must be accepted");
        let kw = generate_working_key(&vec![0u8; key_len], false).unwrap();
        try_decrypt_block(&kw, &inb, &mut outb).expect("real schedule must be accepted");
    }
}

/// A short input or output block raises rather than panicking on the
/// `le_to_u32`/`u32_to_le` index.
#[test]
fn short_block_raises_illegal_argument() {
    let kw = generate_working_key(&[0u8; 16], true).unwrap();
    let mut out16 = [0u8; 16];
    let err = try_encrypt_block(&kw, &[0u8; 15], &mut out16).expect_err("short input");
    assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");

    let mut out15 = [0u8; 15];
    let err = try_encrypt_block(&kw, &[0u8; 16], &mut out15).expect_err("short output");
    assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");
}

/// The infallible pair cannot report a refusal, so it aborts — but it now
/// aborts *deterministically and by name*, before any table lookup, rather
/// than through whichever index happens to run off the end of `kw` first.
/// A 2-entry schedule is the interesting case: it passes the `kw.len() >= 2`
/// lower bound every in-tree caller uses, and the round structure then
/// reads `kw[rounds]` past the end.
#[test]
#[should_panic(expected = "AES engine not initialised")]
fn infallible_encrypt_block_aborts_on_a_two_entry_schedule() {
    let mut outb = [0u8; 16];
    encrypt_block(&vec![[0u32; 4]; 2], &[0u8; 16], &mut outb);
}

#[test]
#[should_panic(expected = "AES engine not initialised")]
fn infallible_decrypt_block_aborts_on_an_empty_schedule() {
    let mut outb = [0u8; 16];
    decrypt_block(&[], &[0u8; 16], &mut outb);
}

#[test]
#[should_panic(expected = "AES block must be 16 bytes")]
fn infallible_encrypt_block_aborts_on_a_short_block() {
    let kw = generate_working_key(&[0u8; 16], true).unwrap();
    let mut outb = [0u8; 16];
    encrypt_block(&kw, &[0u8; 15], &mut outb);
}

/// The fail-loud wrappers must be byte-identical to the infallible pair on
/// every well-formed input — the guard adds a refusal, it does not change
/// the transform.
#[test]
fn try_wrappers_match_the_infallible_pair() {
    let key = unhex("000102030405060708090a0b0c0d0e0f");
    let pt = arr16("00112233445566778899aabbccddeeff");
    let enc = generate_working_key(&key, true).unwrap();

    let mut a = [0u8; 16];
    encrypt_block(&enc, &pt, &mut a);
    let mut b = [0u8; 16];
    try_encrypt_block(&enc, &pt, &mut b).expect("well-formed input");
    assert_eq!(a, b);

    let dec = generate_working_key(&key, false).unwrap();
    let mut c = [0u8; 16];
    decrypt_block(&dec, &a, &mut c);
    let mut d = [0u8; 16];
    try_decrypt_block(&dec, &a, &mut d).expect("well-formed input");
    assert_eq!(c, d);
    assert_eq!(c, pt);
}

#[test]
fn gcm_process_blocks_matches_nist_vector() {
    // NIST SP 800-38D Test Case 4 (128-bit key, 96-bit IV, 60-byte PT -> 3 full blocks = 48 bytes)
    let key = unhex("feffe9928665731c6d6a8f9467308308");
    let kw = generate_working_key(&key, true).unwrap();
    let mut h = [0u8; 16];
    encrypt_block(&kw, &[0u8; 16], &mut h);
    let pt = unhex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525");
    let expected_ct = unhex("42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa05");
    let iv = unhex("cafebabefacedbaddecaf888");
    let mut j0 = [0u8; 16];
    j0[..12].copy_from_slice(&iv);
    j0[15] = 1;
    let mut counter = j0;
    let mut s = [0u8; 16];
    let mut rem = u32::MAX - 1;
    let mut ct = vec![0u8; pt.len()];
    gcm_process_blocks(&kw, &h, &mut s, &mut counter, &mut rem, true, &pt, &mut ct).unwrap();
    assert_eq!(ct, expected_ct);

    let mut dec = vec![0u8; ct.len()];
    let mut counter_dec = j0;
    let mut s_dec = [0u8; 16];
    let mut rem_dec = u32::MAX - 1;
    gcm_process_blocks(&kw, &h, &mut s_dec, &mut counter_dec, &mut rem_dec, false, &ct, &mut dec).unwrap();
    assert_eq!(dec, pt);
    assert_eq!(s_dec, s);
}

#[test]
fn encrypt_ctr_blocks_matches_single_block() {
    let key = unhex("000102030405060708090a0b0c0d0e0f");
    let kw = generate_working_key(&key, true).unwrap();
    let pt = vec![0x5au8; 64];
    let mut out_batch = vec![0u8; 64];
    let mut c_batch = [0u8; 16];
    c_batch[15] = 0xfe;
    encrypt_ctr_blocks(&kw, &mut c_batch, &pt, &mut out_batch);

    let mut out_single = vec![0u8; 64];
    let mut c_single = [0u8; 16];
    c_single[15] = 0xfe;
    let mut ks = [0u8; 16];
    for off in (0..64).step_by(16) {
        encrypt_block(&kw, &c_single, &mut ks);
        for i in 0..16 {
            out_single[off + i] = pt[off + i] ^ ks[i];
        }
        inc_be_counter(&mut c_single);
    }
    assert_eq!(out_batch, out_single);
    assert_eq!(c_batch, c_single);
}
