// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native fast-path for the BouncyCastle ChaCha permutation kernels that
//! dominate the SPHINCS-256 post-quantum scheme.
//!
//! `org.bouncycastle.pqc.crypto.test.RegressionTest` (`Sphincs256Test`) times
//! out (>360 s vs HotSpot ~1.3 s) because, with `org/bouncycastle/*`
//! JIT-banned (the value-model collision in BC's F2m EC path — unrelated to
//! ChaCha), the ChaCha core runs interpreted and its dozens of
//! `Integers.rotateLeft` *method calls* per block crush the interpreter. A
//! fresh stack dump pins the hot leaf:
//!
//! ```text
//! Sphincs256Test.doSHA2KatTest -> SPHINCS256Signer.crypto_sign
//!   -> Horst.horst_sign -> Seed.prg -> Salsa20Engine.processBytes
//!   -> ChaChaEngine.generateKeyStream -> ChaChaEngine.chachaCore (I[I[I)V
//!   -> org/bouncycastle/util/Integers.rotateLeft (II)I        <-- hot leaf
//! ```
//!
//! Two distinct ChaCha permutations appear in SPHINCS-256, with the SAME inner
//! quarter-round structure but different finalization:
//!   * `ChaChaEngine.chachaCore(int rounds, int[] input, int[] x)` — the
//!     stream-cipher block function used by the PRG (`Seed.prg`). Writes
//!     `x[i] = state_i + input[i]` (the standard "add the original input").
//!   * `Permute.permute(int rounds, int[] x)` — the bare permutation used by
//!     the SPHINCS hash (`HashFunctions.hash_2n_n`/`hash_n_n`). Writes
//!     `x[i] = state_i` (NO input-add).
//!
//! Both are `public static` pure functions over `int[16]` with no field or heap
//! access, so a verbatim Rust transcription is bit-identical to the interpreted
//! bytecode by construction (`Integers.rotateLeft(v, n)` for the fixed
//! distances 16/12/8/7 == `v.rotate_left(n)`). Validated below against the
//! canonical RFC 8439 §2.3.2 ChaCha20 block-function known-answer vector.

/// The ChaCha double-round loop, shared by both kernels. Applies `rounds`
/// ChaCha rounds (one loop iteration = a column round + a diagonal round = 2
/// rounds) to the 16-word state in place. Transcribed verbatim from BC's
/// `chachaCore`/`Permute.permute` inner `for (i = rounds; i > 0; i -= 2)` loop.
///
/// `rounds` must be even (BC throws `IllegalArgumentException` otherwise — the
/// caller enforces this before calling). A non-positive `rounds` runs zero
/// iterations, exactly like the Java `for` loop.
#[inline]
fn chacha_rounds(s: &mut [u32; 16], rounds: i32) {
    let mut i = rounds;
    while i > 0 {
        // Column rounds: QR(0,4,8,12) QR(1,5,9,13) QR(2,6,10,14) QR(3,7,11,15)
        s[0] = s[0].wrapping_add(s[4]);   s[12] = (s[12] ^ s[0]).rotate_left(16);
        s[8] = s[8].wrapping_add(s[12]);  s[4]  = (s[4]  ^ s[8]).rotate_left(12);
        s[0] = s[0].wrapping_add(s[4]);   s[12] = (s[12] ^ s[0]).rotate_left(8);
        s[8] = s[8].wrapping_add(s[12]);  s[4]  = (s[4]  ^ s[8]).rotate_left(7);
        s[1] = s[1].wrapping_add(s[5]);   s[13] = (s[13] ^ s[1]).rotate_left(16);
        s[9] = s[9].wrapping_add(s[13]);  s[5]  = (s[5]  ^ s[9]).rotate_left(12);
        s[1] = s[1].wrapping_add(s[5]);   s[13] = (s[13] ^ s[1]).rotate_left(8);
        s[9] = s[9].wrapping_add(s[13]);  s[5]  = (s[5]  ^ s[9]).rotate_left(7);
        s[2] = s[2].wrapping_add(s[6]);   s[14] = (s[14] ^ s[2]).rotate_left(16);
        s[10] = s[10].wrapping_add(s[14]); s[6] = (s[6]  ^ s[10]).rotate_left(12);
        s[2] = s[2].wrapping_add(s[6]);   s[14] = (s[14] ^ s[2]).rotate_left(8);
        s[10] = s[10].wrapping_add(s[14]); s[6] = (s[6]  ^ s[10]).rotate_left(7);
        s[3] = s[3].wrapping_add(s[7]);   s[15] = (s[15] ^ s[3]).rotate_left(16);
        s[11] = s[11].wrapping_add(s[15]); s[7] = (s[7]  ^ s[11]).rotate_left(12);
        s[3] = s[3].wrapping_add(s[7]);   s[15] = (s[15] ^ s[3]).rotate_left(8);
        s[11] = s[11].wrapping_add(s[15]); s[7] = (s[7]  ^ s[11]).rotate_left(7);
        // Diagonal rounds: QR(0,5,10,15) QR(1,6,11,12) QR(2,7,8,13) QR(3,4,9,14)
        s[0] = s[0].wrapping_add(s[5]);   s[15] = (s[15] ^ s[0]).rotate_left(16);
        s[10] = s[10].wrapping_add(s[15]); s[5] = (s[5]  ^ s[10]).rotate_left(12);
        s[0] = s[0].wrapping_add(s[5]);   s[15] = (s[15] ^ s[0]).rotate_left(8);
        s[10] = s[10].wrapping_add(s[15]); s[5] = (s[5]  ^ s[10]).rotate_left(7);
        s[1] = s[1].wrapping_add(s[6]);   s[12] = (s[12] ^ s[1]).rotate_left(16);
        s[11] = s[11].wrapping_add(s[12]); s[6] = (s[6]  ^ s[11]).rotate_left(12);
        s[1] = s[1].wrapping_add(s[6]);   s[12] = (s[12] ^ s[1]).rotate_left(8);
        s[11] = s[11].wrapping_add(s[12]); s[6] = (s[6]  ^ s[11]).rotate_left(7);
        s[2] = s[2].wrapping_add(s[7]);   s[13] = (s[13] ^ s[2]).rotate_left(16);
        s[8] = s[8].wrapping_add(s[13]);  s[7]  = (s[7]  ^ s[8]).rotate_left(12);
        s[2] = s[2].wrapping_add(s[7]);   s[13] = (s[13] ^ s[2]).rotate_left(8);
        s[8] = s[8].wrapping_add(s[13]);  s[7]  = (s[7]  ^ s[8]).rotate_left(7);
        s[3] = s[3].wrapping_add(s[4]);   s[14] = (s[14] ^ s[3]).rotate_left(16);
        s[9] = s[9].wrapping_add(s[14]);  s[4]  = (s[4]  ^ s[9]).rotate_left(12);
        s[3] = s[3].wrapping_add(s[4]);   s[14] = (s[14] ^ s[3]).rotate_left(8);
        s[9] = s[9].wrapping_add(s[14]);  s[4]  = (s[4]  ^ s[9]).rotate_left(7);
        i -= 2;
    }
}

/// `org.bouncycastle.crypto.engines.ChaChaEngine.chachaCore(int rounds,
/// int[] input, int[] x)` — the stream-cipher block function: permute `input`
/// and write `x[i] = state_i + input[i]`. `input` and `x` may be distinct
/// arrays (they are, in `generateKeyStream`). Byte-identical to the bytecode.
pub(crate) fn chacha_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) {
    let mut s = [0u32; 16];
    for k in 0..16 {
        s[k] = input[k] as u32;
    }
    chacha_rounds(&mut s, rounds);
    for k in 0..16 {
        x[k] = s[k].wrapping_add(input[k] as u32) as i32;
    }
}

/// `org.bouncycastle.pqc.crypto.sphincs.Permute.permute(int rounds, int[] x)` —
/// the bare permutation (the SPHINCS hash), updating `x` in place with NO
/// final input-add. Byte-identical to the bytecode.
pub(crate) fn permute(rounds: i32, x: &mut [i32; 16]) {
    let mut s = [0u32; 16];
    for k in 0..16 {
        s[k] = x[k] as u32;
    }
    chacha_rounds(&mut s, rounds);
    for k in 0..16 {
        x[k] = s[k] as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        0xe4e7f110, 0x15593bd1, 0x1fdd0f50, 0xc47120a3,
        0xc7f4d1c7, 0x0368c033, 0x9aaa2204, 0x4e6cd4c3,
        0x466482d2, 0x09aa9f07, 0x05d7c214, 0xa2028bd9,
        0xd19c12b5, 0xb94e16de, 0xe883d0cb, 0x4e3c50a2,
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

    /// `rounds <= 0` runs zero ChaCha rounds, mirroring the Java `for` loop;
    /// `chacha_core` then yields `x[i] = 2*input[i]` (state == input, plus the
    /// input-add) and `permute` is the identity.
    #[test]
    fn zero_rounds_is_loop_skipped() {
        let input: [i32; 16] = RFC8439_INPUT.map(|w| w as i32);
        let mut x = [0i32; 16];
        chacha_core(0, &input, &mut x);
        for k in 0..16 {
            assert_eq!(x[k] as u32, (input[k] as u32).wrapping_mul(2));
        }
        let mut perm = input;
        permute(0, &mut perm);
        assert_eq!(perm, input);
    }
}
