// SPDX-License-Identifier: MIT AND Apache-2.0
//
// Portions of this file are derived from the Bouncy Castle Cryptography Library
// (the ChaCha / Salsa20 permutation kernels, `ChaChaEngine.chachaCore` /
// `Permute.permute`): the quarter-round structure and constant rotation
// distances are mechanically transcribed from BouncyCastle Java source. Those
// portions are
//   Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
// and are used under the Bouncy Castle Licence (an MIT-style permissive licence),
// NOT Apache-2.0. The Craton-authored glue/integration code is
//   Copyright 2024-2026 Craton Software Company (Apache-2.0).
// See ../THIRD-PARTY-NOTICES.md for the full Bouncy Castle Licence text.

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
        s[0] = s[0].wrapping_add(s[4]);
        s[12] = (s[12] ^ s[0]).rotate_left(16);
        s[8] = s[8].wrapping_add(s[12]);
        s[4] = (s[4] ^ s[8]).rotate_left(12);
        s[0] = s[0].wrapping_add(s[4]);
        s[12] = (s[12] ^ s[0]).rotate_left(8);
        s[8] = s[8].wrapping_add(s[12]);
        s[4] = (s[4] ^ s[8]).rotate_left(7);
        s[1] = s[1].wrapping_add(s[5]);
        s[13] = (s[13] ^ s[1]).rotate_left(16);
        s[9] = s[9].wrapping_add(s[13]);
        s[5] = (s[5] ^ s[9]).rotate_left(12);
        s[1] = s[1].wrapping_add(s[5]);
        s[13] = (s[13] ^ s[1]).rotate_left(8);
        s[9] = s[9].wrapping_add(s[13]);
        s[5] = (s[5] ^ s[9]).rotate_left(7);
        s[2] = s[2].wrapping_add(s[6]);
        s[14] = (s[14] ^ s[2]).rotate_left(16);
        s[10] = s[10].wrapping_add(s[14]);
        s[6] = (s[6] ^ s[10]).rotate_left(12);
        s[2] = s[2].wrapping_add(s[6]);
        s[14] = (s[14] ^ s[2]).rotate_left(8);
        s[10] = s[10].wrapping_add(s[14]);
        s[6] = (s[6] ^ s[10]).rotate_left(7);
        s[3] = s[3].wrapping_add(s[7]);
        s[15] = (s[15] ^ s[3]).rotate_left(16);
        s[11] = s[11].wrapping_add(s[15]);
        s[7] = (s[7] ^ s[11]).rotate_left(12);
        s[3] = s[3].wrapping_add(s[7]);
        s[15] = (s[15] ^ s[3]).rotate_left(8);
        s[11] = s[11].wrapping_add(s[15]);
        s[7] = (s[7] ^ s[11]).rotate_left(7);
        // Diagonal rounds: QR(0,5,10,15) QR(1,6,11,12) QR(2,7,8,13) QR(3,4,9,14)
        s[0] = s[0].wrapping_add(s[5]);
        s[15] = (s[15] ^ s[0]).rotate_left(16);
        s[10] = s[10].wrapping_add(s[15]);
        s[5] = (s[5] ^ s[10]).rotate_left(12);
        s[0] = s[0].wrapping_add(s[5]);
        s[15] = (s[15] ^ s[0]).rotate_left(8);
        s[10] = s[10].wrapping_add(s[15]);
        s[5] = (s[5] ^ s[10]).rotate_left(7);
        s[1] = s[1].wrapping_add(s[6]);
        s[12] = (s[12] ^ s[1]).rotate_left(16);
        s[11] = s[11].wrapping_add(s[12]);
        s[6] = (s[6] ^ s[11]).rotate_left(12);
        s[1] = s[1].wrapping_add(s[6]);
        s[12] = (s[12] ^ s[1]).rotate_left(8);
        s[11] = s[11].wrapping_add(s[12]);
        s[6] = (s[6] ^ s[11]).rotate_left(7);
        s[2] = s[2].wrapping_add(s[7]);
        s[13] = (s[13] ^ s[2]).rotate_left(16);
        s[8] = s[8].wrapping_add(s[13]);
        s[7] = (s[7] ^ s[8]).rotate_left(12);
        s[2] = s[2].wrapping_add(s[7]);
        s[13] = (s[13] ^ s[2]).rotate_left(8);
        s[8] = s[8].wrapping_add(s[13]);
        s[7] = (s[7] ^ s[8]).rotate_left(7);
        s[3] = s[3].wrapping_add(s[4]);
        s[14] = (s[14] ^ s[3]).rotate_left(16);
        s[9] = s[9].wrapping_add(s[14]);
        s[4] = (s[4] ^ s[9]).rotate_left(12);
        s[3] = s[3].wrapping_add(s[4]);
        s[14] = (s[14] ^ s[3]).rotate_left(8);
        s[9] = s[9].wrapping_add(s[14]);
        s[4] = (s[4] ^ s[9]).rotate_left(7);
        i -= 2;
    }
}

/// `org.bouncycastle.crypto.engines.ChaChaEngine.chachaCore(int rounds,
/// int[] input, int[] x)` — the stream-cipher block function: permute `input`
/// and write `x[i] = state_i + input[i]`. `input` and `x` may be distinct
/// arrays (they are, in `generateKeyStream`). Byte-identical to the bytecode.
pub fn chacha_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) {
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
/// `org.bouncycastle.crypto.engines.Salsa20Engine.salsaCore(int rounds,
/// int[] input, int[] x)` - the Salsa20 stream-cipher block function.
/// Transcribed from BC's Java source; all additions are Java int wrapping adds.
pub fn salsa_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) {
    let mut x00 = input[0] as u32;
    let mut x01 = input[1] as u32;
    let mut x02 = input[2] as u32;
    let mut x03 = input[3] as u32;
    let mut x04 = input[4] as u32;
    let mut x05 = input[5] as u32;
    let mut x06 = input[6] as u32;
    let mut x07 = input[7] as u32;
    let mut x08 = input[8] as u32;
    let mut x09 = input[9] as u32;
    let mut x10 = input[10] as u32;
    let mut x11 = input[11] as u32;
    let mut x12 = input[12] as u32;
    let mut x13 = input[13] as u32;
    let mut x14 = input[14] as u32;
    let mut x15 = input[15] as u32;

    let mut i = rounds;
    while i > 0 {
        x04 ^= x00.wrapping_add(x12).rotate_left(7);
        x08 ^= x04.wrapping_add(x00).rotate_left(9);
        x12 ^= x08.wrapping_add(x04).rotate_left(13);
        x00 ^= x12.wrapping_add(x08).rotate_left(18);
        x09 ^= x05.wrapping_add(x01).rotate_left(7);
        x13 ^= x09.wrapping_add(x05).rotate_left(9);
        x01 ^= x13.wrapping_add(x09).rotate_left(13);
        x05 ^= x01.wrapping_add(x13).rotate_left(18);
        x14 ^= x10.wrapping_add(x06).rotate_left(7);
        x02 ^= x14.wrapping_add(x10).rotate_left(9);
        x06 ^= x02.wrapping_add(x14).rotate_left(13);
        x10 ^= x06.wrapping_add(x02).rotate_left(18);
        x03 ^= x15.wrapping_add(x11).rotate_left(7);
        x07 ^= x03.wrapping_add(x15).rotate_left(9);
        x11 ^= x07.wrapping_add(x03).rotate_left(13);
        x15 ^= x11.wrapping_add(x07).rotate_left(18);

        x01 ^= x00.wrapping_add(x03).rotate_left(7);
        x02 ^= x01.wrapping_add(x00).rotate_left(9);
        x03 ^= x02.wrapping_add(x01).rotate_left(13);
        x00 ^= x03.wrapping_add(x02).rotate_left(18);
        x06 ^= x05.wrapping_add(x04).rotate_left(7);
        x07 ^= x06.wrapping_add(x05).rotate_left(9);
        x04 ^= x07.wrapping_add(x06).rotate_left(13);
        x05 ^= x04.wrapping_add(x07).rotate_left(18);
        x11 ^= x10.wrapping_add(x09).rotate_left(7);
        x08 ^= x11.wrapping_add(x10).rotate_left(9);
        x09 ^= x08.wrapping_add(x11).rotate_left(13);
        x10 ^= x09.wrapping_add(x08).rotate_left(18);
        x12 ^= x15.wrapping_add(x14).rotate_left(7);
        x13 ^= x12.wrapping_add(x15).rotate_left(9);
        x14 ^= x13.wrapping_add(x12).rotate_left(13);
        x15 ^= x14.wrapping_add(x13).rotate_left(18);

        i -= 2;
    }

    let out = [
        x00, x01, x02, x03, x04, x05, x06, x07, x08, x09, x10, x11, x12, x13, x14, x15,
    ];
    for k in 0..16 {
        x[k] = out[k].wrapping_add(input[k] as u32) as i32;
    }
}

pub fn permute(rounds: i32, x: &mut [i32; 16]) {
    let mut s = [0u32; 16];
    for k in 0..16 {
        s[k] = x[k] as u32;
    }
    chacha_rounds(&mut s, rounds);
    for k in 0..16 {
        x[k] = s[k] as i32;
    }
}

/// The fixed round count of `Permute.chacha_permute` (BC's
/// `Permute.CHACHA_ROUNDS`).
pub(crate) const SPHINCS_CHACHA_ROUNDS: i32 = 12;

/// `org.bouncycastle.pqc.crypto.sphincs.Permute.chacha_permute(byte[] out,
/// byte[] in)` — the SPHINCS hash leaf (`HashFunctions.hash_n_n`/`hash_2n_n`),
/// by far the hottest frame in tree/WOTS signing. Reads 16 little-endian words
/// from `input`, runs the 12-round permutation, and writes them back
/// little-endian to `out`. Folds in the per-call `new int[16]` allocation and
/// the 32 `Pack.littleEndianToInt`/`intToLittleEndian` conversions that the
/// bytecode does around `permute`. `out` and `input` may alias — the callers
/// pass `chacha_permute(x, x)` — so all input is consumed before any output is
/// written. Byte-identical to the bytecode (`Pack` little-endian == LE bytes).
pub fn chacha_permute_bytes(out: &mut [u8; 64], input: &[u8; 64]) {
    let mut s = [0u32; 16];
    for k in 0..16 {
        s[k] = u32::from_le_bytes([
            input[4 * k],
            input[4 * k + 1],
            input[4 * k + 2],
            input[4 * k + 3],
        ]);
    }
    chacha_rounds(&mut s, SPHINCS_CHACHA_ROUNDS);
    for k in 0..16 {
        out[4 * k..4 * k + 4].copy_from_slice(&s[k].to_le_bytes());
    }
}

/// `HashFunctions.hashc` — the 32-byte ChaCha state suffix.
pub(crate) const SPHINCS_HASHC: [u8; 32] = *b"expand 32-byte to 64-byte state!";

/// `HashFunctions.hash_n_n`: `out = chacha_permute(in32 || hashc)[0..32]`.
/// (The SPHINCS one-block hash; `HASH_BYTES == 32`.)
pub fn sphincs_hash_n_n(in32: &[u8; 32]) -> [u8; 32] {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(in32);
    x[32..].copy_from_slice(&SPHINCS_HASHC);
    let mut y = [0u8; 64];
    chacha_permute_bytes(&mut y, &x);
    let mut out = [0u8; 32];
    out.copy_from_slice(&y[..32]);
    out
}

/// `HashFunctions.hash_2n_n`: two-block compression `in64 -> out32`.
/// `x = in[0..32] || hashc; permute(x); x[0..32] ^= in[32..64]; permute(x);
///  out = x[0..32]`.
pub fn sphincs_hash_2n_n(in64: &[u8; 64]) -> [u8; 32] {
    let mut x = [0u8; 64];
    x[..32].copy_from_slice(&in64[..32]);
    x[32..].copy_from_slice(&SPHINCS_HASHC);
    let mut y = [0u8; 64];
    chacha_permute_bytes(&mut y, &x); // first permute
    for i in 0..32 {
        y[i] ^= in64[32 + i];
    }
    let mut z = [0u8; 64];
    chacha_permute_bytes(&mut z, &y); // second permute
    let mut out = [0u8; 32];
    out.copy_from_slice(&z[..32]);
    out
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
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
}
