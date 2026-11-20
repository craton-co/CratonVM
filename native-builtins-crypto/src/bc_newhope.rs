// SPDX-License-Identifier: MIT AND GPL-2.0-only WITH Classpath-exception-2.0
//
// Portions of this file are derived from the Bouncy Castle Cryptography Library
// (the NewHope lattice kernels `NTT` / `Reduce` / `Poly`): the number-theoretic
// transform, Montgomery reduction, and polynomial arithmetic are mechanically
// transcribed from BouncyCastle Java source. Those portions are
//   Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.
// and are used under the Bouncy Castle Licence (an MIT-style permissive licence).
// The Craton-authored glue/integration code is
//   Copyright 2024-2026 Craton Software Company (GPL-2.0-only WITH Classpath-exception-2.0).
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

#[cfg(target_arch = "x86_64")]
mod avx512_ntt {
    use super::*;
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "avx512f")]
    unsafe fn montgomery_16(a: __m512i) -> __m256i {
        let qinv = _mm512_set1_epi32(QINV);
        let rmask = _mm512_set1_epi32(RMASK);
        let q = _mm512_set1_epi32(Q);

        let u = _mm512_mullo_epi32(a, qinv);
        let u = _mm512_and_epi32(u, rmask);
        let u = _mm512_mullo_epi32(u, q);
        let u = _mm512_add_epi32(u, a);
        let res = _mm512_srli_epi32(u, RLOG);
        _mm512_cvtepi32_epi16(res)
    }

    #[inline]
    #[target_feature(enable = "avx512f,avx512bw")]
    unsafe fn barrett_16(a: __m256i) -> __m256i {
        let five = _mm256_set1_epi16(5);
        let q = _mm256_set1_epi16(Q as i16);
        let u = _mm256_mulhi_epu16(a, five);
        let u = _mm256_mullo_epi16(u, q);
        _mm256_sub_epi16(a, u)
    }

    #[inline]
    #[target_feature(enable = "avx512f,avx512bw")]
    unsafe fn barrett_32(a: __m512i) -> __m512i {
        let five = _mm512_set1_epi16(5);
        let q = _mm512_set1_epi16(Q as i16);
        let u = _mm512_mulhi_epu16(a, five);
        let u = _mm512_mullo_epi16(u, q);
        _mm512_sub_epi16(a, u)
    }

    #[target_feature(enable = "avx512f,avx512bw")]
    pub unsafe fn mul_coefficients(poly: &mut [i16; N], factors: &[i16; N]) {
        for i in (0..N).step_by(32) {
            let x_lo = _mm512_cvtepu16_epi32(_mm256_loadu_si256(poly.as_ptr().add(i) as *const __m256i));
            let x_hi = _mm512_cvtepu16_epi32(_mm256_loadu_si256(poly.as_ptr().add(i + 16) as *const __m256i));
            let y_lo = _mm512_cvtepu16_epi32(_mm256_loadu_si256(factors.as_ptr().add(i) as *const __m256i));
            let y_hi = _mm512_cvtepu16_epi32(_mm256_loadu_si256(factors.as_ptr().add(i + 16) as *const __m256i));

            let prod_lo = _mm512_mullo_epi32(x_lo, y_lo);
            let prod_hi = _mm512_mullo_epi32(x_hi, y_hi);

            let res_lo = montgomery_16(prod_lo);
            let res_hi = montgomery_16(prod_hi);

            _mm256_storeu_si256(poly.as_mut_ptr().add(i) as *mut __m256i, res_lo);
            _mm256_storeu_si256(poly.as_mut_ptr().add(i + 16) as *mut __m256i, res_hi);
        }
    }

    #[target_feature(enable = "avx512f,avx512bw")]
    pub unsafe fn core(a: &mut [i16; N], omega: &[i16; 512]) {
        let mut i = 0;
        while i < 10 {
            // Even level
            let distance = 1usize << i;
            let num_blocks = N / (2 * distance);
            if distance < 16 {
                for k in 0..num_blocks {
                    let w = omega[k] as i32;
                    let base = k * 2 * distance;
                    for start in 0..distance {
                        let j = base + start;
                        let u = (a[j] as u16) as i32;
                        let v = (a[j + distance] as u16) as i32;
                        a[j] = (u + v) as i16;
                        a[j + distance] = montgomery(w.wrapping_mul(u + 3 * Q - v));
                    }
                }
            } else if distance == 16 {
                let three_q = _mm512_set1_epi32(3 * Q);
                for k in 0..num_blocks {
                    let w_vec = _mm512_set1_epi32(omega[k] as i32);
                    let base = k * 32;
                    let u_16 = _mm256_loadu_si256(a.as_ptr().add(base) as *const __m256i);
                    let v_16 = _mm256_loadu_si256(a.as_ptr().add(base + 16) as *const __m256i);

                    let sum_16 = _mm256_add_epi16(u_16, v_16);
                    _mm256_storeu_si256(a.as_mut_ptr().add(base) as *mut __m256i, sum_16);

                    let u_32 = _mm512_cvtepu16_epi32(u_16);
                    let v_32 = _mm512_cvtepu16_epi32(v_16);
                    let diff = _mm512_sub_epi32(_mm512_add_epi32(u_32, three_q), v_32);
                    let prod = _mm512_mullo_epi32(w_vec, diff);
                    let res_16 = montgomery_16(prod);
                    _mm256_storeu_si256(a.as_mut_ptr().add(base + 16) as *mut __m256i, res_16);
                }
            } else {
                let three_q = _mm512_set1_epi32(3 * Q);
                for k in 0..num_blocks {
                    let w_vec = _mm512_set1_epi32(omega[k] as i32);
                    let base = k * 2 * distance;
                    for start in (0..distance).step_by(32) {
                        let j = base + start;
                        let u_512 = _mm512_loadu_si512(a.as_ptr().add(j) as *const __m512i);
                        let v_512 = _mm512_loadu_si512(a.as_ptr().add(j + distance) as *const __m512i);

                        let sum_512 = _mm512_add_epi16(u_512, v_512);
                        _mm512_storeu_si512(a.as_mut_ptr().add(j) as *mut __m512i, sum_512);

                        let u_lo = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(u_512));
                        let u_hi = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(u_512, 1));
                        let v_lo = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(v_512));
                        let v_hi = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(v_512, 1));

                        let diff_lo = _mm512_sub_epi32(_mm512_add_epi32(u_lo, three_q), v_lo);
                        let diff_hi = _mm512_sub_epi32(_mm512_add_epi32(u_hi, three_q), v_hi);

                        let prod_lo = _mm512_mullo_epi32(w_vec, diff_lo);
                        let prod_hi = _mm512_mullo_epi32(w_vec, diff_hi);

                        let res_lo = montgomery_16(prod_lo);
                        let res_hi = montgomery_16(prod_hi);

                        _mm256_storeu_si256(a.as_mut_ptr().add(j + distance) as *mut __m256i, res_lo);
                        _mm256_storeu_si256(a.as_mut_ptr().add(j + distance + 16) as *mut __m256i, res_hi);
                    }
                }
            }

            // Odd level
            let distance = distance << 1;
            let num_blocks = N / (2 * distance);
            if distance < 16 {
                for k in 0..num_blocks {
                    let w = omega[k] as i32;
                    let base = k * 2 * distance;
                    for start in 0..distance {
                        let j = base + start;
                        let u = (a[j] as u16) as i32;
                        let v = (a[j + distance] as u16) as i32;
                        a[j] = barrett((u + v) as i16);
                        a[j + distance] = montgomery(w.wrapping_mul(u + 3 * Q - v));
                    }
                }
            } else if distance == 16 {
                let three_q = _mm512_set1_epi32(3 * Q);
                for k in 0..num_blocks {
                    let w_vec = _mm512_set1_epi32(omega[k] as i32);
                    let base = k * 32;
                    let u_16 = _mm256_loadu_si256(a.as_ptr().add(base) as *const __m256i);
                    let v_16 = _mm256_loadu_si256(a.as_ptr().add(base + 16) as *const __m256i);

                    let sum_16 = barrett_16(_mm256_add_epi16(u_16, v_16));
                    _mm256_storeu_si256(a.as_mut_ptr().add(base) as *mut __m256i, sum_16);

                    let u_32 = _mm512_cvtepu16_epi32(u_16);
                    let v_32 = _mm512_cvtepu16_epi32(v_16);
                    let diff = _mm512_sub_epi32(_mm512_add_epi32(u_32, three_q), v_32);
                    let prod = _mm512_mullo_epi32(w_vec, diff);
                    let res_16 = montgomery_16(prod);
                    _mm256_storeu_si256(a.as_mut_ptr().add(base + 16) as *mut __m256i, res_16);
                }
            } else {
                let three_q = _mm512_set1_epi32(3 * Q);
                for k in 0..num_blocks {
                    let w_vec = _mm512_set1_epi32(omega[k] as i32);
                    let base = k * 2 * distance;
                    for start in (0..distance).step_by(32) {
                        let j = base + start;
                        let u_512 = _mm512_loadu_si512(a.as_ptr().add(j) as *const __m512i);
                        let v_512 = _mm512_loadu_si512(a.as_ptr().add(j + distance) as *const __m512i);

                        let sum_512 = barrett_32(_mm512_add_epi16(u_512, v_512));
                        _mm512_storeu_si512(a.as_mut_ptr().add(j) as *mut __m512i, sum_512);

                        let u_lo = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(u_512));
                        let u_hi = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(u_512, 1));
                        let v_lo = _mm512_cvtepu16_epi32(_mm512_castsi512_si256(v_512));
                        let v_hi = _mm512_cvtepu16_epi32(_mm512_extracti64x4_epi64(v_512, 1));

                        let diff_lo = _mm512_sub_epi32(_mm512_add_epi32(u_lo, three_q), v_lo);
                        let diff_hi = _mm512_sub_epi32(_mm512_add_epi32(u_hi, three_q), v_hi);

                        let prod_lo = _mm512_mullo_epi32(w_vec, diff_lo);
                        let prod_hi = _mm512_mullo_epi32(w_vec, diff_hi);

                        let res_lo = montgomery_16(prod_lo);
                        let res_hi = montgomery_16(prod_hi);

                        _mm256_storeu_si256(a.as_mut_ptr().add(j + distance) as *mut __m256i, res_lo);
                        _mm256_storeu_si256(a.as_mut_ptr().add(j + distance + 16) as *mut __m256i, res_hi);
                    }
                }
            }
            i += 2;
        }
    }
}

/// `NTT.core(short[] a, short[] omega)` — the GS butterfly. `omega` is the
/// 512-entry twiddle table (`OMEGAS_*_MONTGOMERY`). Reads are `& 0xFFFF`
/// (unsigned); the even level skips reduction ("be lazy"), the odd level
/// barrett-reduces the sum.
#[inline]
fn core(a: &mut [i16; N], omega: &[i16; 512]) {
    #[cfg(target_arch = "x86_64")]
    if crate::avx512::pqc_enabled() {
        unsafe {
            avx512_ntt::core(a, omega);
        }
        return;
    }
    core_scalar(a, omega);
}

fn core_scalar(a: &mut [i16; N], omega: &[i16; 512]) {
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
        i += 2;    }}

/// `NTT.mulCoefficients(short[] poly, short[] factors)`. `xi * yi` overflows
/// `int` (both are `& 0xFFFF`, up to 65535) → wrapping multiply.
#[inline]
fn mul_coefficients(poly: &mut [i16; N], factors: &[i16; N]) {
    #[cfg(target_arch = "x86_64")]
    if crate::avx512::pqc_enabled() {
        unsafe {
            avx512_ntt::mul_coefficients(poly, factors);
        }
        return;
    }
    mul_coefficients_scalar(poly, factors);
}

fn mul_coefficients_scalar(poly: &mut [i16; N], factors: &[i16; N]) {
    for i in 0..N {
        let xi = (poly[i] as u16) as i32;
        let yi = (factors[i] as u16) as i32;
        poly[i] = montgomery(xi.wrapping_mul(yi));
    }
}

/// `Poly.toNTT(short[] r)`.
pub fn to_ntt(r: &mut [i16; N]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    mul_coefficients(r, &PSIS_BITREV_MONTGOMERY);
    core(r, &OMEGAS_MONTGOMERY);
}

/// `Poly.fromNTT(short[] r)`.
pub fn from_ntt(r: &mut [i16; N]) {
    let _scrub = crate::scrub::ScrubOnDrop;
    bit_reverse(r);
    core(r, &OMEGAS_INV_MONTGOMERY);
    mul_coefficients(r, &PSIS_INV_MONTGOMERY);
}

/// `Poly.uniform(short[] a, byte[] seed)` — generate the public `a` polynomial
/// by rejection-sampling SHAKE128(seed) into `[0, 5*Q)`. `doOutput(256)` in BC
/// squeezes the next 256 bytes of the XOF stream, which `XofReader::read`
/// mirrors. Fills exactly `N` coefficients.
pub fn uniform(a: &mut [i16; N], seed: &[u8]) {
    let _scrub = crate::scrub::ScrubOnDrop;
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
                    return;                }            }
            i += 2;
        }
    }
}

#[cfg(test)]
mod tests;
