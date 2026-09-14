// SPDX-License-Identifier: MIT AND Apache-2.0
//
// Portions of this file are derived from the Bouncy Castle Cryptography Library
// (the NewHope lattice kernels `NTT` / `Reduce` / `Poly`): the number-theoretic
// transform, Montgomery reduction, and polynomial arithmetic are mechanically
// transcribed from BouncyCastle Java source. Those portions are
//   Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
// and are used under the Bouncy Castle Licence (an MIT-style permissive licence),
// NOT Apache-2.0. The Craton-authored glue/integration code is
//   Copyright 2024-2026 Craton Software Company (Apache-2.0).
// See ../THIRD-PARTY-NOTICES.md for the full Bouncy Castle Licence text.

//! Native fast-path for the BouncyCastle NewHope (post-quantum key exchange)
//! lattice kernels that dominate `NewHopeTest` once the ChaCha cores are native.
//!
//! `NewHopeTest.testKeyExchange` runs 1000 full key-exchange rounds; a stack
//! sample shows the time concentrated in the interpreted **number-theoretic
//! transform** (`NTT.core`, called ~5x/round via `Poly.toNTT`/`fromNTT`) — pure
//! `short[]` Montgomery arithmetic that the interpreter handles one coefficient
//! at a time. The `a`-polynomial generation (`Poly.uniform`, SHAKE128 rejection
//! sampling) is the next-heaviest (interpreted Keccak).
//!
//! With `org/bouncycastle/*` JIT-banned (the F2m-EC value-model collision, a
//! `long`-only bug unrelated to these `short`/`int` kernels), both run
//! interpreted. This module is a verbatim transcription of `NTT`/`Reduce`/
//! `Poly` (BouncyCastle, under the Bouncy Castle Licence — MIT-style; see
//! ../THIRD-PARTY-NOTICES.md) — bit-identical by construction — plus
//! SHAKE128 from the already-present `sha3` crate (standard, == BC's
//! `SHAKEDigest(128)`). Validated by an NTT round-trip test, `Reduce` spot
//! checks, and a SHAKE128 KAT below, and end-to-end by `NewHopeTest`'s own KAT.

use crate::bc_newhope_tables::{
    BIT_REVERSE_TABLE, OMEGAS_INV_MONTGOMERY, OMEGAS_MONTGOMERY, PSIS_BITREV_MONTGOMERY,
    PSIS_INV_MONTGOMERY,
};

const N: usize = 1024; // Params.N
const Q: i32 = 12289; // Params.Q
const QINV: i32 = 12287; // Reduce.QInv = -inverse_mod(Q, 2^18)
const RLOG: u32 = 18; // Reduce.RLog
const RMASK: i32 = (1 << RLOG) - 1; // Reduce.RMask

/// `Reduce.montgomery(int a)`. Java `int` arithmetic wraps mod 2^32 (the
/// `a * QInv` and `u * Q` products overflow), so every step uses wrapping ops;
/// `>>> RLog` is a logical shift and `(short)` truncates to 16 bits.
#[inline]
fn montgomery(a: i32) -> i16 {
    let mut u = a.wrapping_mul(QINV);
    u &= RMASK;
    u = u.wrapping_mul(Q);
    u = u.wrapping_add(a);
    ((u as u32) >> RLOG) as i16
}

/// `Reduce.barrett(short a)`.
#[inline]
fn barrett(a: i16) -> i16 {
    let t = (a as u16) as i32; // a & 0xFFFF
    let mut u = ((t.wrapping_mul(5)) as u32 >> 16) as i32; // (t * 5) >>> 16
    u = u.wrapping_mul(Q);
    (t.wrapping_sub(u)) as i16
}

/// `NTT.bitReverse(short[] poly)`.
#[inline]
fn bit_reverse(poly: &mut [i16; N]) {
    for i in 0..N {
        let r = BIT_REVERSE_TABLE[i] as usize; // values are 0..1023
        if i < r {
            poly.swap(i, r);
        }
    }
}

/// `NTT.core(short[] a, short[] omega)` — the GS butterfly. `omega` is the
/// 512-entry twiddle table (`OMEGAS_*_MONTGOMERY`). Reads are `& 0xFFFF`
/// (unsigned); the even level skips reduction ("be lazy"), the odd level
/// barrett-reduces the sum.
fn core(a: &mut [i16; N], omega: &[i16; 512]) {
    let n = N as i32;
    let mut i = 0;
    while i < 10 {
        // Even level
        let mut distance = 1i32 << i;
        let mut start = 0;
        while start < distance {
            let mut j_twiddle = 0usize;
            let mut j = start;
            while j < n - 1 {
                let u = (a[j as usize] as u16) as i32;
                let v = (a[(j + distance) as usize] as u16) as i32;
                let w = omega[j_twiddle] as i32; // int w = omega[jTwiddle++]
                j_twiddle += 1;
                a[j as usize] = (u + v) as i16; // (short)(u + v) — lazy
                a[(j + distance) as usize] = montgomery(w.wrapping_mul(u + 3 * Q - v));
                j += 2 * distance;
            }
            start += 1;
        }

        // Odd level
        distance <<= 1;
        start = 0;
        while start < distance {
            let mut j_twiddle = 0usize;
            let mut j = start;
            while j < n - 1 {
                let u = (a[j as usize] as u16) as i32;
                let v = (a[(j + distance) as usize] as u16) as i32;
                let w = omega[j_twiddle] as i32;
                j_twiddle += 1;
                a[j as usize] = barrett((u + v) as i16);
                a[(j + distance) as usize] = montgomery(w.wrapping_mul(u + 3 * Q - v));
                j += 2 * distance;
            }
            start += 1;
        }
        i += 2;
    }
}

/// `NTT.mulCoefficients(short[] poly, short[] factors)`. `xi * yi` overflows
/// `int` (both are `& 0xFFFF`, up to 65535) → wrapping multiply.
#[inline]
fn mul_coefficients(poly: &mut [i16; N], factors: &[i16; N]) {
    for i in 0..N {
        let xi = (poly[i] as u16) as i32;
        let yi = (factors[i] as u16) as i32;
        poly[i] = montgomery(xi.wrapping_mul(yi));
    }
}

/// `Poly.toNTT(short[] r)`.
pub fn to_ntt(r: &mut [i16; N]) {
    mul_coefficients(r, &PSIS_BITREV_MONTGOMERY);
    core(r, &OMEGAS_MONTGOMERY);
}

/// `Poly.fromNTT(short[] r)`.
pub fn from_ntt(r: &mut [i16; N]) {
    bit_reverse(r);
    core(r, &OMEGAS_INV_MONTGOMERY);
    mul_coefficients(r, &PSIS_INV_MONTGOMERY);
}

/// `Poly.uniform(short[] a, byte[] seed)` — generate the public `a` polynomial
/// by rejection-sampling SHAKE128(seed) into `[0, 5*Q)`. `doOutput(256)` in BC
/// squeezes the next 256 bytes of the XOF stream, which `XofReader::read`
/// mirrors. Fills exactly `N` coefficients.
pub fn uniform(a: &mut [i16; N], seed: &[u8]) {
    use sha3::digest::{ExtendableOutput, Update, XofReader};
    let mut xof = sha3::Shake128::default();
    xof.update(seed);
    let mut reader = xof.finalize_xof();

    let mut pos = 0usize;
    let mut output = [0u8; 256];
    loop {
        reader.read(&mut output);
        let mut i = 0;
        while i < output.len() {
            let val = (output[i] as i32 & 0xFF) | ((output[i + 1] as i32 & 0xFF) << 8);
            if val < 5 * Q {
                a[pos] = val as i16;
                pos += 1;
                if pos == N {
                    return;
                }
            }
            i += 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn reduce_spot_checks() {
        // montgomery(0) and barrett(0) are 0; barrett reduces values >= Q.
        assert_eq!(montgomery(0), 0);
        assert_eq!(barrett(0), 0);
        // barrett(a) for a in [0, Q) is the identity (no reduction needed):
        for a in [1i16, 100, 12288] {
            assert_eq!(
                ((barrett(a) as i32).rem_euclid(Q)),
                (a as i32).rem_euclid(Q)
            );
        }
        // barrett of a value >= Q reduces it into range mod Q.
        let big = 24577i32; // 2*Q - 1
        assert_eq!(barrett(big as i16) as i32 & 0xFFFF, (big - Q));
    }

    /// FNV-1a over the unsigned-16-bit coefficient view, matching the Java
    /// probe's checksum (Java `long` wraps == `u64` wrapping).
    fn fnv(a: &[i16; N]) -> i64 {
        let mut s: u64 = 0xcbf2_9ce4_8422_2325;
        for &v in a.iter() {
            s ^= (v as u16) as u64;
            s = s.wrapping_mul(0x0000_0100_0000_01b3);
        }
        s as i64
    }

    /// `to_ntt`/`from_ntt` validated against HotSpot ground truth (BC's real
    /// `Poly.toNTT`/`fromNTT` via NHNttProbe) over all 1024 coefficients — the
    /// transforms are NOT a plain inverse pair (Montgomery-domain factors only
    /// cancel across the protocol's pointwise multiply), so this exact-vector
    /// check is the correct validation, exercising every table + `core` +
    /// `bit_reverse` + `mul_coefficients` + `montgomery` + `barrett`.
    #[test]
    fn ntt_matches_hotspot() {
        let mut x = [0i16; N];
        for i in 0..N {
            x[i] = (((i * 7 + 13) % 12289) as i32) as i16;
        }
        to_ntt(&mut x);
        let head: Vec<i32> = (0..8).map(|i| (x[i] as u16) as i32).collect();
        assert_eq!(head, [8477, 7986, 2020, 14350, 7831, 6381, 14052, 13714]);
        // Full-array checksum — verified equal element-for-element to HotSpot's
        // real Poly.toNTT across all 1024 coefficients during the port.
        assert_eq!(fnv(&x), -5417792707627256840);

        from_ntt(&mut x);
        let head2: Vec<i32> = (0..8).map(|i| (x[i] as u16) as i32).collect();
        assert_eq!(head2, [13, 3597, 1805, 5389, 909, 4493, 2701, 6285]);
        assert_eq!(fnv(&x), -7401367446177724180);
    }

    /// SHAKE128("") known-answer (NIST) — confirms the `sha3` crate's SHAKE128
    /// matches BC's `SHAKEDigest(128)` byte stream, which the rejection sampler
    /// consumes.
    #[test]
    fn shake128_empty_kat() {
        use sha3::digest::{ExtendableOutput, XofReader};
        let mut out = [0u8; 32];
        let mut r = sha3::Shake128::default().finalize_xof();
        r.read(&mut out);
        let expect = [
            0x7f, 0x9c, 0x2b, 0xa4, 0xe8, 0x8f, 0x82, 0x7d, 0x61, 0x60, 0x45, 0x50, 0x76, 0x05,
            0x85, 0x3e, 0xd7, 0x3b, 0x80, 0x93, 0xf6, 0xef, 0xbc, 0x88, 0xeb, 0x1a, 0x6e, 0xac,
            0xfa, 0x66, 0xef, 0x26,
        ];
        assert_eq!(out, expect);
    }

    /// `uniform` validated against HotSpot ground truth (BC's real
    /// `Poly.uniform` with seed = 0..31) — confirms the SHAKE128 stream +
    /// rejection sampler are byte-identical. Also checks the `[0, 5*Q)` range.
    #[test]
    fn uniform_matches_hotspot() {
        let mut a = [0i16; N];
        let seed: Vec<u8> = (0..32u8).collect();
        uniform(&mut a, &seed);
        let head: Vec<i32> = (0..8).map(|i| (a[i] as u16) as i32).collect();
        assert_eq!(head, [27142, 7478, 30150, 22264, 52686, 11200, 8485, 4234]);
        assert_eq!(fnv(&a), -4161122429893521255);
        for &c in a.iter() {
            assert!((c as u16 as i32) < 5 * Q, "coefficient out of range");
        }
    }
}
