// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Step 1 of the limb-based `java.math.BigInteger` rewrite — see
//! `docs/biginteger-limb-rewrite-scope.md`.
//!
//! A self-contained signed arbitrary-precision integer over base-2^32
//! little-endian magnitude words — the same words HotSpot/CratonVM already
//! store in the `mag:[I` field, so later steps can read/write that layout
//! directly instead of round-tripping through O(digits^2) decimal strings.
//!
//! This step is **purely additive**: nothing in the VM routes through `BigInt`
//! yet. It exists so the migration (steps 3-6 of the scope doc) has a tested
//! foundation. Every operation here is validated against the existing decimal
//! `bi_*_str` primitives in the differential tests at the bottom of the file
//! (the differential safety net the earlier reverted fast-path attempt lacked).
#![allow(dead_code)]

use std::cmp::Ordering;

/// Signed arbitrary-precision integer.
///
/// `mag` is the little-endian base-2^32 magnitude, **normalized**: it has no
/// trailing zero limbs, and the empty vector is the canonical zero. `neg` is
/// always `false` when the value is zero, so equality is structural.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

impl BigInt {
    pub(crate) fn zero() -> Self {
        BigInt { neg: false, mag: Vec::new() }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub(crate) fn is_neg(&self) -> bool {
        self.neg
    }

    pub(crate) fn signum(&self) -> i32 {
        if self.mag.is_empty() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    /// Canonicalize: strip trailing zero limbs and force a positive sign on
    /// zero. The single constructor that enforces the invariant.
    fn normalize(mut mag: Vec<u32>, neg: bool) -> Self {
        while mag.last() == Some(&0) {
            mag.pop();
        }
        if mag.is_empty() {
            BigInt { neg: false, mag }
        } else {
            BigInt { neg, mag }
        }
    }

    // -----------------------------------------------------------------
    // mag:[I read/write boundary (the point of the whole rewrite)
    // -----------------------------------------------------------------

    /// Build from a sign and little-endian base-2^32 magnitude words. `words`
    /// need not be normalized. (The `mag:[I` field is big-endian, so callers
    /// reverse it before calling — kept here as pure little-endian to match
    /// the internal representation and the unit tests.)
    pub(crate) fn from_le_words(neg: bool, words: Vec<u32>) -> Self {
        Self::normalize(words, neg)
    }

    /// Borrow the normalized little-endian magnitude words.
    pub(crate) fn mag_le(&self) -> &[u32] {
        &self.mag
    }

    // -----------------------------------------------------------------
    // decimal boundary (only for String <-> BigInteger + tests)
    // -----------------------------------------------------------------

    pub(crate) fn from_decimal(s: &str) -> Self {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let mut mag: Vec<u32> = Vec::new();
        for ch in digits.bytes() {
            if !ch.is_ascii_digit() {
                continue;
            }
            // mag = mag * 10 + digit
            let mut carry = (ch - b'0') as u64;
            for w in mag.iter_mut() {
                let v = (*w as u64) * 10 + carry;
                *w = v as u32;
                carry = v >> 32;
            }
            while carry > 0 {
                mag.push(carry as u32);
                carry >>= 32;
            }
        }
        Self::normalize(mag, neg)
    }

    pub(crate) fn to_decimal(&self) -> String {
        if self.mag.is_empty() {
            return "0".to_string();
        }
        const CHUNK: u64 = 1_000_000_000;
        let mut work = self.mag.clone();
        let mut chunks: Vec<u32> = Vec::new();
        while !work.is_empty() {
            let mut rem: u64 = 0;
            for i in (0..work.len()).rev() {
                let cur = (rem << 32) | (work[i] as u64);
                work[i] = (cur / CHUNK) as u32;
                rem = cur % CHUNK;
            }
            while work.last() == Some(&0) {
                work.pop();
            }
            chunks.push(rem as u32);
        }
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        for (i, c) in chunks.iter().rev().enumerate() {
            if i == 0 {
                out.push_str(&c.to_string());
            } else {
                out.push_str(&format!("{c:09}"));
            }
        }
        out
    }

    // -----------------------------------------------------------------
    // unsigned magnitude primitives
    // -----------------------------------------------------------------

    fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
        if a.len() != b.len() {
            return a.len().cmp(&b.len());
        }
        for i in (0..a.len()).rev() {
            if a[i] != b[i] {
                return a[i].cmp(&b[i]);
            }
        }
        Ordering::Equal
    }

    fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let n = a.len().max(b.len());
        let mut out = Vec::with_capacity(n + 1);
        let mut carry = 0u64;
        for i in 0..n {
            let av = *a.get(i).unwrap_or(&0) as u64;
            let bv = *b.get(i).unwrap_or(&0) as u64;
            let s = av + bv + carry;
            out.push(s as u32);
            carry = s >> 32;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }

    /// `a - b`, assuming `a >= b` as magnitudes. Result is not normalized.
    fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let av = a[i] as i64;
            let bv = *b.get(i).unwrap_or(&0) as i64;
            let mut d = av - bv - borrow;
            if d < 0 {
                d += 1i64 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(d as u32);
        }
        out
    }

    fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
        if a.is_empty() || b.is_empty() {
            return Vec::new();
        }
        let mut out = vec![0u32; a.len() + b.len()];
        for (i, &ai) in a.iter().enumerate() {
            if ai == 0 {
                continue;
            }
            let mut carry = 0u64;
            for (j, &bj) in b.iter().enumerate() {
                let idx = i + j;
                let v = (ai as u64) * (bj as u64) + out[idx] as u64 + carry;
                out[idx] = v as u32;
                carry = v >> 32;
            }
            // Propagate the final carry. The product of `a.len()+b.len()` words
            // fits in `a.len()+b.len()` words, so this never runs off the end.
            let mut k = i + b.len();
            while carry > 0 {
                let v = out[k] as u64 + carry;
                out[k] = v as u32;
                carry = v >> 32;
                k += 1;
            }
        }
        out
    }

    /// Logical left shift of a magnitude by `n` bits.
    fn shl_mag(mag: &[u32], n: usize) -> Vec<u32> {
        if mag.is_empty() {
            return Vec::new();
        }
        let word_shift = n / 32;
        let bit_shift = (n % 32) as u32;
        let mut out = vec![0u32; word_shift];
        if bit_shift == 0 {
            out.extend_from_slice(mag);
        } else {
            let mut carry = 0u64;
            for &w in mag {
                let v = ((w as u64) << bit_shift) | carry;
                out.push(v as u32);
                carry = v >> 32;
            }
            if carry > 0 {
                out.push(carry as u32);
            }
        }
        out
    }

    /// Logical right shift of a magnitude by `n` bits. Returns the shifted
    /// magnitude and whether any set bit was shifted out (the "remainder",
    /// needed for arithmetic (floor) shift of negative values).
    fn shr_mag(mag: &[u32], n: usize) -> (Vec<u32>, bool) {
        let word_shift = n / 32;
        let bit_shift = (n % 32) as u32;
        if word_shift >= mag.len() {
            let lost = mag.iter().any(|&w| w != 0);
            return (Vec::new(), lost);
        }
        let mut lost = mag[..word_shift].iter().any(|&w| w != 0);
        if bit_shift != 0 && (mag[word_shift] & ((1u32 << bit_shift) - 1)) != 0 {
            lost = true;
        }
        let hi = &mag[word_shift..];
        let mut out = Vec::with_capacity(hi.len());
        if bit_shift == 0 {
            out.extend_from_slice(hi);
        } else {
            for i in 0..hi.len() {
                let lo = hi[i] >> bit_shift;
                let carry = if i + 1 < hi.len() {
                    hi[i + 1] << (32 - bit_shift)
                } else {
                    0
                };
                out.push(lo | carry);
            }
        }
        (out, lost)
    }

    // -----------------------------------------------------------------
    // signed arithmetic
    // -----------------------------------------------------------------

    pub(crate) fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => Self::cmp_mag(&self.mag, &o.mag),
            (true, true) => Self::cmp_mag(&o.mag, &self.mag),
        }
    }

    pub(crate) fn neg_value(&self) -> BigInt {
        if self.is_zero() {
            BigInt::zero()
        } else {
            BigInt { neg: !self.neg, mag: self.mag.clone() }
        }
    }

    pub(crate) fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            Self::normalize(Self::add_mag(&self.mag, &o.mag), self.neg)
        } else {
            match Self::cmp_mag(&self.mag, &o.mag) {
                Ordering::Equal => BigInt::zero(),
                Ordering::Greater => {
                    Self::normalize(Self::sub_mag(&self.mag, &o.mag), self.neg)
                }
                Ordering::Less => {
                    Self::normalize(Self::sub_mag(&o.mag, &self.mag), o.neg)
                }
            }
        }
    }

    pub(crate) fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.neg_value())
    }

    pub(crate) fn mul(&self, o: &BigInt) -> BigInt {
        if self.is_zero() || o.is_zero() {
            return BigInt::zero();
        }
        Self::normalize(Self::mul_mag(&self.mag, &o.mag), self.neg != o.neg)
    }

    /// Arithmetic (two's-complement, floor-toward-negative-infinity) left shift
    /// — i.e. `value * 2^n`. Matches `BigInteger.shiftLeft`.
    pub(crate) fn shl(&self, n: u32) -> BigInt {
        if self.is_zero() || n == 0 {
            return self.clone();
        }
        Self::normalize(Self::shl_mag(&self.mag, n as usize), self.neg)
    }

    /// Arithmetic right shift — `floor(value / 2^n)`. Matches
    /// `BigInteger.shiftRight`: for negative values this rounds toward negative
    /// infinity (so `-1 >> n == -1`, `-3 >> 1 == -2`), realized as
    /// `-ceil(|value| / 2^n)`.
    pub(crate) fn shr(&self, n: u32) -> BigInt {
        if self.is_zero() || n == 0 {
            return self.clone();
        }
        let (mut q, lost) = Self::shr_mag(&self.mag, n as usize);
        if self.neg {
            // floor division of a negative = -ceildiv(|value|, 2^n)
            //                              = -(floor(|value|/2^n) + [remainder>0])
            if lost {
                q = Self::add_mag(&q, &[1]);
            }
            // ceildiv(|value|>=1, 2^n) >= 1, so the magnitude is never zero here.
            Self::normalize(q, true)
        } else {
            Self::normalize(q, false)
        }
    }

    // -----------------------------------------------------------------
    // division / remainder / modulo
    // -----------------------------------------------------------------

    /// In-place `r = (r << 1) | bit`.
    fn shl1_or_mag(r: &mut Vec<u32>, bit: u32) {
        let mut carry = (bit & 1) as u64;
        for limb in r.iter_mut() {
            let v = ((*limb as u64) << 1) | carry;
            *limb = v as u32;
            carry = v >> 32;
        }
        if carry > 0 {
            r.push(carry as u32);
        }
    }

    /// Unsigned magnitude division: `(quotient, remainder) = a divmod b`.
    /// `b` must be non-empty (non-zero). Both results are normalized.
    ///
    /// Binary long division (process `a` MSB→LSB, shifting into a running
    /// remainder). O(bits(a) · limbs) — far faster than the decimal
    /// repeated-subtraction `bi_div_unsigned`, and exact. (Knuth Algorithm D /
    /// Montgomery are the later perf-polish steps in the scope doc; this is the
    /// correctness-first foundation step 3 routes through.)
    fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
        debug_assert!(!b.is_empty(), "divmod_mag: zero divisor");
        if Self::cmp_mag(a, b) == Ordering::Less {
            let mut r = a.to_vec();
            while r.last() == Some(&0) {
                r.pop();
            }
            return (Vec::new(), r);
        }
        let total_bits = a.len() * 32;
        let mut q = vec![0u32; a.len()];
        let mut r: Vec<u32> = Vec::new();
        for bit_idx in (0..total_bits).rev() {
            let bit = (a[bit_idx / 32] >> (bit_idx % 32)) & 1;
            Self::shl1_or_mag(&mut r, bit);
            if Self::cmp_mag(&r, b) != Ordering::Less {
                r = Self::sub_mag(&r, b);
                while r.last() == Some(&0) {
                    r.pop();
                }
                q[bit_idx / 32] |= 1u32 << (bit_idx % 32);
            }
        }
        while q.last() == Some(&0) {
            q.pop();
        }
        while r.last() == Some(&0) {
            r.pop();
        }
        (q, r)
    }

    /// Truncated quotient (rounds toward zero) — `BigInteger.divide`. Sign is
    /// `sign(self) * sign(o)`. Returns zero for a zero divisor (matching the
    /// decimal reference; the native layer raises ArithmeticException upstream).
    pub(crate) fn div(&self, o: &BigInt) -> BigInt {
        if o.is_zero() || self.is_zero() {
            return BigInt::zero();
        }
        let (q, _) = Self::divmod_mag(&self.mag, &o.mag);
        Self::normalize(q, self.neg != o.neg)
    }

    /// Remainder with the sign of the dividend — `BigInteger.remainder`.
    pub(crate) fn rem(&self, o: &BigInt) -> BigInt {
        if o.is_zero() || self.is_zero() {
            return BigInt::zero();
        }
        let (_, r) = Self::divmod_mag(&self.mag, &o.mag);
        Self::normalize(r, self.neg)
    }

    /// Truncated `(quotient, remainder)` together — `BigInteger.divideAndRemainder`.
    pub(crate) fn divmod(&self, o: &BigInt) -> (BigInt, BigInt) {
        if o.is_zero() || self.is_zero() {
            return (BigInt::zero(), BigInt::zero());
        }
        let (q, r) = Self::divmod_mag(&self.mag, &o.mag);
        (
            Self::normalize(q, self.neg != o.neg),
            Self::normalize(r, self.neg),
        )
    }

    /// Non-negative result in `[0, |o|)` — `BigInteger.mod` (real BigInteger
    /// requires a positive modulus; we reduce against `|o|`).
    pub(crate) fn modulo(&self, o: &BigInt) -> BigInt {
        if o.is_zero() {
            return BigInt::zero();
        }
        let r = self.rem(o);
        if r.is_neg() {
            // r in (-|o|, 0): the non-negative representative is |o| - |r|.
            Self::normalize(Self::sub_mag(&o.mag, &r.mag), false)
        } else {
            r
        }
    }
}

// ---------------------------------------------------------------------------
// Differential tests against the decimal `bi_*_str` reference primitives.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bi_add_str, bi_compare, bi_div_str, bi_mod_str, bi_mul_str, bi_shift_left_str,
        bi_shift_right_str, bi_sub_str,
    };

    // Deterministic LCG so the spread is reproducible without a rand dep.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    /// Random signed decimal string, 1..=48 digits, ~half negative.
    fn rand_decimal(state: &mut u64) -> String {
        let len = (lcg(state) % 48) as usize + 1;
        let mut s = String::new();
        if lcg(state) & 1 == 0 {
            s.push('-');
        }
        // First digit 1..=9 (no leading zero).
        s.push((b'0' + 1 + (lcg(state) % 9) as u8) as char);
        for _ in 1..len {
            s.push((b'0' + (lcg(state) % 10) as u8) as char);
        }
        s
    }

    fn edge_cases() -> Vec<String> {
        vec![
            "0", "1", "-1", "2", "-2", "7", "-7", "10", "-10",
            "4294967295", "4294967296", "4294967297", "-4294967296",
            "18446744073709551615", "18446744073709551616",
            "340282366920938463463374607431768211456", // 2^128
            "115792089237316195423570985008687907853269984665640564039457584007913129639936", // 2^256
            "-115792089237316195423570985008687907853269984665640564039457584007913129639936",
            "999999999999999999999999999999",
            "-999999999999999999999999999999",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn b(s: &str) -> BigInt {
        BigInt::from_decimal(s)
    }

    #[test]
    fn decimal_roundtrip_and_le_words() {
        for s in edge_cases() {
            let v = b(&s);
            // Canonical decimal round-trips (HotSpot canonicalizes "+0" → "0").
            assert_eq!(v.to_decimal(), s, "decimal roundtrip {s}");
            // le-words → BigInt reproduces the same value.
            let rebuilt = BigInt::from_le_words(v.is_neg(), v.mag_le().to_vec());
            assert_eq!(rebuilt, v, "le-words roundtrip {s}");
        }
        // Spot-check the actual little-endian words for a known value:
        // 2^32 + 1 == [1, 1] little-endian.
        assert_eq!(b("4294967297").mag_le(), &[1u32, 1u32]);
        assert_eq!(b("0").mag_le(), &[] as &[u32]);
        assert_eq!(b("4294967296").mag_le(), &[0u32, 1u32]);
    }

    #[test]
    fn add_sub_mul_cmp_match_decimal() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut operands = edge_cases();
        for _ in 0..400 {
            operands.push(rand_decimal(&mut state));
        }
        // All edge × edge pairs + a sweep of random pairs.
        let n = operands.len();
        for i in 0..n {
            for j in 0..n {
                // Keep the full matrix for the (small) edge set; sample the
                // random tail to bound runtime.
                if i >= 20 && (i + j) % 7 != 0 {
                    continue;
                }
                let a = &operands[i];
                let c = &operands[j];
                let ba = b(a);
                let bc = b(c);
                assert_eq!(ba.add(&bc).to_decimal(), bi_add_str(a, c), "add {a}+{c}");
                assert_eq!(ba.sub(&bc).to_decimal(), bi_sub_str(a, c), "sub {a}-{c}");
                assert_eq!(ba.mul(&bc).to_decimal(), bi_mul_str(a, c), "mul {a}*{c}");
                assert_eq!(
                    ba.cmp(&bc),
                    bi_compare(a, c).cmp(&0),
                    "cmp {a} ? {c}"
                );
            }
        }
    }

    #[test]
    fn div_rem_mod_match_decimal() {
        let mut state = 0x0bad_c0de_1357_9bdfu64;
        let mut operands = edge_cases();
        for _ in 0..400 {
            operands.push(rand_decimal(&mut state));
        }
        let n = operands.len();
        for i in 0..n {
            for j in 0..n {
                if i >= 20 && (i + j) % 7 != 0 {
                    continue;
                }
                let a = &operands[i];
                let c = &operands[j];
                if c == "0" {
                    continue; // divide-by-zero raised upstream; not exercised here
                }
                let ba = b(a);
                let bc = b(c);

                // Truncated divide + sign-of-dividend remainder vs the decimal
                // reference.
                assert_eq!(ba.div(&bc).to_decimal(), bi_div_str(a, c), "div {a}/{c}");
                assert_eq!(ba.rem(&bc).to_decimal(), bi_mod_str(a, c), "rem {a}%{c}");

                // divmod agrees with the separate div/rem.
                let (q, r) = ba.divmod(&bc);
                assert_eq!(q, ba.div(&bc), "divmod q {a}/{c}");
                assert_eq!(r, ba.rem(&bc), "divmod r {a}%{c}");

                // Fundamental identity: a == q*c + r.
                assert_eq!(q.mul(&bc).add(&r), ba, "q*c+r==a for {a},{c}");
                // |r| < |c|.
                assert_eq!(
                    BigInt::cmp_mag(r.mag_le(), bc.mag_le()),
                    Ordering::Less,
                    "|r| < |c| for {a},{c}"
                );

                // BigInteger.mod is non-negative and == (rem + |c|) % |c|.
                let m = ba.modulo(&bc);
                assert!(!m.is_neg(), "mod non-negative {a} mod {c}");
                let want_mod = {
                    let c_abs = c.trim_start_matches('-');
                    let rr = bi_mod_str(a, c_abs);
                    if let Some(stripped) = rr.strip_prefix('-') {
                        if stripped == "0" { "0".to_string() } else { bi_add_str(&rr, c_abs) }
                    } else {
                        rr
                    }
                };
                assert_eq!(m.to_decimal(), want_mod, "mod {a} mod {c}");
            }
        }
    }

    #[test]
    fn shifts_match_decimal() {
        let mut state = 0xfeed_face_dead_beefu64;
        let mut operands = edge_cases();
        for _ in 0..200 {
            operands.push(rand_decimal(&mut state));
        }
        let shifts: [u32; 9] = [0, 1, 2, 7, 31, 32, 33, 64, 130];
        for a in &operands {
            let ba = b(a);
            for &s in &shifts {
                assert_eq!(
                    ba.shl(s).to_decimal(),
                    bi_shift_left_str(a, s as i32),
                    "shl {a} << {s}"
                );
                assert_eq!(
                    ba.shr(s).to_decimal(),
                    bi_shift_right_str(a, s as i32),
                    "shr {a} >> {s}"
                );
            }
        }
    }
}
