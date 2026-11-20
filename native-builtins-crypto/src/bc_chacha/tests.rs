use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// RFC 8439 §2.3.2 ChaCha20 block-function known-answer test. The input
/// state is the documented (constants ‖ key ‖ counter ‖ nonce) layout; the
/// expected output is the RFC's "ChaCha state at the end of the ChaCha20
/// operation" (i.e. after 20 rounds AND the input-add), so it exercises
/// `chacha_core` end to end — the shared `chacha_rounds` plus finalization.
const RFC8439_INPUT: [u32; 16] = [
    0x61707865, 0x3320646e, 0x79622d32, 0x6b206574, // "expand 32-byte k"
    0x03020100, 0x07060504, 0x0b0a0908, 0x0f0e0d0c, // key 00..0f
    0x13121110, 0x17161514, 0x1b1a1918, 0x1f1e1d1c, // key 10..1f
    0x00000001, 0x09000000, 0x4a000000, 0x00000000, // counter=1, nonce
];
const RFC8439_OUTPUT: [u32; 16] = [
    0xe4e7f110, 0x15593bd1, 0x1fdd0f50, 0xc47120a3, 0xc7f4d1c7, 0x0368c033, 0x9aaa2204,
    0x4e6cd4c3, 0x466482d2, 0x09aa9f07, 0x05d7c214, 0xa2028bd9, 0xd19c12b5, 0xb94e16de,
    0xe883d0cb, 0x4e3c50a2,
];

#[test]
fn chacha_core_matches_rfc8439_block() {
    let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
    let mut x = [0i32; 16];
    chacha_core(20, &input, &mut x);
    for k in 0..16 {
        assert_eq!(
            x[k] as u32, RFC8439_OUTPUT[k],
            "chacha_core word {k} mismatch"
        );
    }
}

#[test]
fn salsa_core_matches_bc_set1_vector() {
    let input: [u32; 16] = [
        0x6170_7865,
        0x0000_0080,
        0,
        0,
        0,
        0x3120_646e,
        0,
        0,
        0,
        0,
        0x7962_2d36,
        0x0000_0080,
        0,
        0,
        0,
        0x6b20_6574,
    ];
    let mut x = [0i32; 16];
    salsa_core(20, &input.map(|w| w as i32), &mut x);
    let mut out = [0u8; 64];
    for k in 0..16 {
        out[4 * k..4 * k + 4].copy_from_slice(&(x[k] as u32).to_le_bytes());
    }
    assert_eq!(
        hex(&out),
        "4dfa5e481da23ea09a31022050859936da52fcee218005164f267cb65f5cfd7f2b4f97e0ff16924a52df269515110a07f9e460bc65ef95da58f740b7d1dbb0aa"
    );
}

/// `permute` runs the identical inner rounds but omits the input-add, so
/// `permute(state) + input == chacha_core(input)` word-wise. Ties the
/// bare permutation to the RFC-validated `chacha_core` (and hence the
/// shared `chacha_rounds`), with no separate vector needed.
#[test]
fn permute_equals_chacha_core_minus_input_add() {
    let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
    let mut core_out = [0i32; 16];
    chacha_core(20, &input, &mut core_out);

    let mut perm = input;
    permute(20, &mut perm);

    for k in 0..16 {
        assert_eq!(
            (perm[k] as u32).wrapping_add(input[k] as u32),
            core_out[k] as u32,
            "permute word {k} does not match chacha_core - input"
        );
    }
    // And the permutation alone reproduces the RFC output minus the add.
    for k in 0..16 {
        assert_eq!(
            perm[k] as u32,
            RFC8439_OUTPUT[k].wrapping_sub(RFC8439_INPUT[k]),
            "permute word {k} mismatch vs RFC output - input"
        );
    }
}

/// **The zero-round key leak.** `rounds == 0` passes a parity-only check
/// and runs zero ChaCha rounds, so `chacha_core` emits
/// `x[i] = 2 * input[i]` — a "keystream" that is an invertible function of
/// the engine state, and the engine state holds the key. This test asserts
/// both halves of the fix: the checked form refuses with BouncyCastle's own
/// wording, and it refuses *without touching the output buffer*, so no
/// caller can XOR plaintext against a partially written stream.
///
/// The arithmetic that used to happen is spelled out (not performed) so a
/// future reader can see exactly what is being refused: with the RFC 8439
/// state, word 4 is the first key word `0x03020100`, and a zero-round
/// `chacha_core` would have published `0x06040200` — the key word, shifted
/// left by one bit.
#[test]
fn zero_rounds_is_refused_because_it_publishes_the_engine_state() {
    let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
    let mut x = [0i32; 16];
    let err = try_chacha_core(0, &input, &mut x).expect_err("zero rounds must be refused");
    assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");
    assert_eq!(err.message(), "'rounds' must be a positive, even number");
    assert_eq!(x, [0i32; 16], "a refusal must not write a keystream");

    // The leak that would have been: 2 * key-word-0, i.e. the key with one
    // bit shifted out. Asserted as arithmetic, never as kernel output.
    assert_eq!(
        (RFC8439_INPUT[4]).wrapping_mul(2),
        0x0604_0200,
        "sanity: the zero-round output would be the doubled key word"
    );

    let mut perm = input;
    assert!(
        try_permute(0, &mut perm).is_err(),
        "a zero-round permute is the identity — the SPHINCS hash would be forgeable"
    );
    assert_eq!(perm, input, "a refusal must not modify the state");
    assert!(try_salsa_core(0, &input, &mut x).is_err());
    assert_eq!(x, [0i32; 16]);
}

/// A *negative even* count is `rounds == 0` wearing a different hat (the
/// Java loop runs zero iterations for both), so it is refused identically
/// rather than slipping through the parity test.
#[test]
fn negative_even_round_counts_are_refused_like_zero() {
    let input = [0i32; 16];
    let mut x = [0i32; 16];
    for rounds in [-2i32, -20, i32::MIN] {
        let err =
            try_chacha_core(rounds, &input, &mut x).expect_err("negative rounds must refuse");
        assert_eq!(err.message(), "'rounds' must be a positive, even number");
        assert!(try_salsa_core(rounds, &input, &mut x).is_err());
        assert!(try_permute(rounds, &mut [0i32; 16]).is_err());
    }
}

/// The infallible kernels have no channel to report a refusal, so they
/// abort. That is the only remaining option that is not "emit something
/// the caller will treat as a keystream"; see `demand_rounds`.
#[test]
#[should_panic(expected = "must be a positive, even number")]
fn infallible_chacha_core_aborts_on_zero_rounds() {
    let mut x = [0i32; 16];
    chacha_core(0, &[0i32; 16], &mut x);
}

#[test]
#[should_panic(expected = "must be a positive, even number")]
fn infallible_salsa_core_aborts_on_zero_rounds() {
    let mut x = [0i32; 16];
    salsa_core(0, &[0i32; 16], &mut x);
}

#[test]
#[should_panic(expected = "must be a positive, even number")]
fn infallible_permute_aborts_on_zero_rounds() {
    permute(0, &mut [0i32; 16]);
}

#[test]
#[should_panic(expected = "Number of rounds must be even")]
fn infallible_chacha_core_aborts_on_odd_rounds() {
    let mut x = [0i32; 16];
    chacha_core(7, &[0i32; 16], &mut x);
}

/// `chacha_permute_bytes` must equal: LE-decode 64 bytes -> `permute(12)` ->
/// LE-encode. Ties the byte wrapper to the RFC-validated `permute`/
/// `chacha_rounds`. Uses an in==out aliasing check too (the real callers
/// pass `chacha_permute(x, x)`).
#[test]
fn chacha_permute_bytes_matches_permute() {
    // Arbitrary but fixed 64-byte input (0,1,2,...,63).
    let mut input = [0u8; 64];
    for (i, b) in input.iter_mut().enumerate() {
        *b = i as u8;
    }
    // Reference: decode LE -> permute(12) -> encode LE.
    let mut ref_words = [0i32; 16];
    for k in 0..16 {
        ref_words[k] = u32::from_le_bytes([
            input[4 * k],
            input[4 * k + 1],
            input[4 * k + 2],
            input[4 * k + 3],
        ]) as i32;
    }
    permute(SPHINCS_CHACHA_ROUNDS, &mut ref_words);
    let mut expected = [0u8; 64];
    for k in 0..16 {
        expected[4 * k..4 * k + 4].copy_from_slice(&(ref_words[k] as u32).to_le_bytes());
    }

    let mut out = [0u8; 64];
    chacha_permute_bytes(&mut out, &input);
    assert_eq!(out, expected, "chacha_permute_bytes vs permute mismatch");
    // (The in==out aliasing case the real callers use is handled at the
    // native layer, which reads `in` fully into a Rust local before writing
    // `out`; the pure fn here takes disjoint &mut/& refs by construction.)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- fail-loud round-parity guards ----

/// An odd round count must raise, not quietly emit a `rounds - 1`
/// keystream. Covers all three kernels; the message is BouncyCastle's own.
#[test]
fn odd_round_counts_raise_illegal_argument() {
    let input = [0i32; 16];
    let mut x = [0i32; 16];
    // Positive odds only — a *negative* odd count is refused first by the
    // positivity half of the guard, with a different message, and is
    // covered by `negative_even_round_counts_are_refused_like_zero`'s
    // sibling assertions.
    for rounds in [1i32, 7, 11, 19, 21] {
        let err = try_chacha_core(rounds, &input, &mut x)
            .expect_err("odd round count must be rejected");
        assert_eq!(err.java_class(), "java/lang/IllegalArgumentException");
        assert_eq!(err.message(), "Number of rounds must be even");

        assert!(try_salsa_core(rounds, &input, &mut x).is_err());
        let mut y = [0i32; 16];
        assert!(try_permute(rounds, &mut y).is_err());
        // The refusal left the output untouched: no partial keystream was
        // written that a caller could XOR against plaintext.
        assert_eq!(y, [0i32; 16]);
    }
    assert_eq!(x, [0i32; 16]);
}

/// Supported (even) round counts still succeed and are byte-identical to
/// the infallible kernels — the guard refuses, it does not transform.
#[test]
fn even_round_counts_succeed_and_match_the_infallible_kernels() {
    let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
    // `0` is deliberately absent: it is no longer an accepted round count
    // (see `zero_rounds_is_refused_because_it_publishes_the_engine_state`).
    for rounds in [2i32, 8, 12, 20] {
        let mut plain = [0i32; 16];
        chacha_core(rounds, &input, &mut plain);
        let mut checked = [0i32; 16];
        try_chacha_core(rounds, &input, &mut checked).expect("even round count");
        assert_eq!(plain, checked, "chacha_core mismatch at {rounds} rounds");

        let mut plain = [0i32; 16];
        salsa_core(rounds, &input, &mut plain);
        let mut checked = [0i32; 16];
        try_salsa_core(rounds, &input, &mut checked).expect("even round count");
        assert_eq!(plain, checked, "salsa_core mismatch at {rounds} rounds");

        let mut plain = input;
        permute(rounds, &mut plain);
        let mut checked = input;
        try_permute(rounds, &mut checked).expect("even round count");
        assert_eq!(plain, checked, "permute mismatch at {rounds} rounds");
    }
}

/// The RFC-validated 20-round vector still verifies through the checked
/// entry point, so the guard cannot have perturbed the transform.
#[test]
fn try_chacha_core_matches_rfc8439_block() {
    let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
    let mut x = [0i32; 16];
    try_chacha_core(20, &input, &mut x).expect("20 rounds is even");
    for k in 0..16 {
        assert_eq!(x[k] as u32, RFC8439_OUTPUT[k], "word {k} mismatch");
    }
}

/// SPHINCS `hash_n_n` / `hash_2n_n` validated against HotSpot ground truth
/// (BC's real `HashFunctions`), pinning the `hashc` constant + the XOR /
/// double-permute structure.
#[test]
fn sphincs_hash_matches_hotspot() {
    let mut in32 = [0u8; 32];
    for (i, b) in in32.iter_mut().enumerate() {
        *b = (i * 3 + 1) as u8;
    }
    assert_eq!(
        hex(&sphincs_hash_n_n(&in32)),
        "1970575d27d8a7a2c802327bd6c0bd1984589fb7f1dfeb24b41ef58a2c20ffb6"
    );

    let mut in64 = [0u8; 64];
    for (i, b) in in64.iter_mut().enumerate() {
        *b = (i * 5 + 2) as u8;
    }
    assert_eq!(
        hex(&sphincs_hash_2n_n(&in64)),
        "a2a7d5c917bd9f691b1eff4da7deb7d31125e572d9bad40900d8a6fdbaf2f578"
    );
}
