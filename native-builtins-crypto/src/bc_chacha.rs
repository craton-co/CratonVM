// SPDX-License-Identifier: MIT AND GPL-2.0-only WITH Classpath-exception-2.0
//
// Portions of this file are derived from the Bouncy Castle Cryptography Library
// (the ChaCha / Salsa20 permutation kernels, `ChaChaEngine.chachaCore` /
// `Permute.permute`): the quarter-round structure and constant rotation
// distances are mechanically transcribed from BouncyCastle Java source. Those
// portions are
//   Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
// and are used under the Bouncy Castle Licence (an MIT-style permissive licence).
// The Craton-authored glue/integration code is
//   Copyright 2024-2026 Craton Software Company (GPL-2.0-only WITH Classpath-exception-2.0).
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

use crate::failure::{CryptoFailure, CryptoResult};

/// BouncyCastle's own admission rule for a round count. `Salsa20Engine`'s
/// constructor is the authority:
///
/// ```text
/// if (rounds <= 0 || (rounds & 1) != 0) {
///     throw new IllegalArgumentException("'rounds' must be a positive, even number");
/// }
/// ```
///
/// **Both halves matter, and only the parity half used to be checked here.**
///
/// * *Odd* — the `for (i = rounds; i > 0; i -= 2)` loop simply runs one round
///   fewer, so an odd count produces a *plausible but wrong* keystream: no
///   error, no crash, just a stream that will not interoperate and whose
///   security properties are not the ones ChaCha was analysed under.
///
/// * *Non-positive* — far worse, and the reason this guard was tightened.
///   `rounds == 0` passes a parity-only check (`0 % 2 == 0`) and runs **zero**
///   permutation rounds, so `chacha_core`/`salsa_core` emit
///   `x[i] = 2 * input[i]` — a "keystream" that is an invertible function of
///   the engine state, and the engine state *is the key*. XOR that against
///   plaintext and the key falls out. `permute` with zero rounds is the
///   identity, which makes the SPHINCS hash trivially forgeable.
///
///   Zero is not a hypothetical value. The
///   `Salsa20Engine.processBytes` native reads its round count out of the Java
///   object by name — `native-builtins/src/phases_late/bouncycastle.rs:6994` —
///   and a by-name read of an unwritten `int` slot yields `Value::Int(0)`,
///   which that `match` accepts as a genuine round count. Refusing it here is
///   the backstop.
///
/// A *negative even* count is also refused: the Java loop would run zero
/// iterations, i.e. it is `rounds == 0` wearing a different hat.
fn check_rounds(rounds: i32) -> CryptoResult<()> {
    if rounds <= 0 {
        return Err(CryptoFailure::illegal_argument(
            "'rounds' must be a positive, even number",
        ));
    }
    if rounds % 2 != 0 {
        return Err(CryptoFailure::illegal_argument(
            "Number of rounds must be even",
        ));
    }
    Ok(())
}

/// The guard applied by the **infallible** kernels, which have no way to return
/// a failure to their caller.
///
/// A `-> ()` kernel that cannot report a refusal has exactly three options for
/// a round count it must not honour: emit the wrong keystream (silent, and the
/// zero-round case leaks the key), leave the output buffer untouched (silent,
/// and the caller then XORs against a stale or zero keystream — plaintext in
/// the clear), or abort. Only the third is a refusal, so that is what this
/// does. The panic message carries BouncyCastle's own wording.
///
/// No live path is expected to reach it. Of the six in-tree call sites, four
/// already validate the round count themselves — the three registered natives
/// re-check parity (`native-builtins/src/phases_late/bouncycastle.rs:7233`,
/// `:7260`, `:7284`) and `bc_scrypt_block_mix` (`:9385`) passes the literal
/// `8`. The remaining two are the `bc_stream_generate_key_stream` arms
/// (`:6890`, `:6894`), which validate nothing and take the field-read value
/// described on [`check_rounds`]; `Salsa20Engine`/`ChaChaEngine` reject a bad
/// round count in their Java constructors, which is the only thing standing
/// between that read and this guard. Prefer the `try_*` forms, which report
/// the same conditions as a catchable Java exception.
#[inline]
fn demand_rounds(rounds: i32, kernel: &str) {
    if let Err(e) = check_rounds(rounds) {
        panic!("{kernel}: refusing to run with rounds={rounds} — {e}");
    }
}

/// Fail-loud [`chacha_core`]: rejects a round count that is odd (silently
/// running `rounds - 1` rounds) **or** non-positive (silently publishing the
/// engine state as a keystream). See [`check_rounds`].
///
/// The in-tree callers re-check *parity* before invoking the infallible form
/// (`native-builtins/src/phases_late/bouncycastle.rs:7233`); none of them
/// checks positivity, so that half of the guard exists only here.
pub fn try_chacha_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) -> CryptoResult<()> {
    check_rounds(rounds)?;
    chacha_core(rounds, input, x);
    Ok(())
}

/// Fail-loud [`salsa_core`]. Guard mirrors
/// `native-builtins/src/phases_late/bouncycastle.rs:7260`.
pub fn try_salsa_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) -> CryptoResult<()> {
    check_rounds(rounds)?;
    salsa_core(rounds, input, x);
    Ok(())
}

/// Fail-loud [`permute`]. Guard mirrors
/// `native-builtins/src/phases_late/bouncycastle.rs:7284`.
pub fn try_permute(rounds: i32, x: &mut [i32; 16]) -> CryptoResult<()> {
    check_rounds(rounds)?;
    permute(rounds, x);
    Ok(())
}

/// The ChaCha double-round loop, shared by both kernels. Applies `rounds`
/// ChaCha rounds (one loop iteration = a column round + a diagonal round = 2
/// rounds) to the 16-word state in place. Transcribed verbatim from BC's
/// `chachaCore`/`Permute.permute` inner `for (i = rounds; i > 0; i -= 2)` loop.
///
/// `rounds` must be positive and even. This function itself does not check —
/// it is a private helper and every entry point that reaches it has already
/// gone through [`check_rounds`] (the `try_*` wrappers) or [`demand_rounds`]
/// (the infallible kernels), except [`chacha_permute_bytes`], which passes the
/// compile-time constant [`SPHINCS_CHACHA_ROUNDS`]. A non-positive `rounds`
/// here would run zero iterations, exactly like the Java `for` loop — which is
/// precisely the leak [`check_rounds`] documents, so do not add a caller that
/// skips the guard.
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
///
/// # Panics
///
/// If `rounds` is not a positive even number. See [`demand_rounds`] for why
/// the refusal is an abort rather than a degraded keystream, and prefer
/// [`try_chacha_core`], which reports the same condition as a catchable Java
/// `IllegalArgumentException`.
pub fn chacha_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    demand_rounds(rounds, "ChaChaEngine.chachaCore");
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
///
/// # Panics
///
/// If `rounds` is not a positive even number — see [`chacha_core`].
pub fn salsa_core(rounds: i32, input: &[i32; 16], x: &mut [i32; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    demand_rounds(rounds, "Salsa20Engine.salsaCore");
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

///
/// # Panics
///
/// If `rounds` is not a positive even number — see [`chacha_core`]. A
/// zero-round `permute` is the *identity*, which would make the SPHINCS hash
/// trivially forgeable, so it is refused rather than performed.
pub fn permute(rounds: i32, x: &mut [i32; 16]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    demand_rounds(rounds, "Permute.permute");
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
    let _scrub = crate::scrub::ScrubOnDrop;
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
mod tests;
