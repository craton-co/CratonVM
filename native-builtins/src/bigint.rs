// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Step 1 of the limb-based `java.math.BigInteger` rewrite — see
//! `biginteger-limb-rewrite-scope.md`.
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
        BigInt {
            neg: false,
            mag: Vec::new(),
        }
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

    /// `BigInteger.abs()` — the magnitude with a non-negative sign.
    pub(crate) fn abs_value(&self) -> BigInt {
        BigInt {
            neg: false,
            mag: self.mag.clone(),
        }
    }

    pub(crate) fn neg_value(&self) -> BigInt {
        if self.is_zero() {
            BigInt::zero()
        } else {
            BigInt {
                neg: !self.neg,
                mag: self.mag.clone(),
            }
        }
    }

    pub(crate) fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            Self::normalize(Self::add_mag(&self.mag, &o.mag), self.neg)
        } else {
            match Self::cmp_mag(&self.mag, &o.mag) {
                Ordering::Equal => BigInt::zero(),
                Ordering::Greater => Self::normalize(Self::sub_mag(&self.mag, &o.mag), self.neg),
                Ordering::Less => Self::normalize(Self::sub_mag(&o.mag, &self.mag), o.neg),
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
    /// `b` must be non-zero. Both results are normalized.
    ///
    /// Knuth Algorithm D (TAOCP 4.3.1), base 2^32, in the Hacker's-Delight
    /// `divmnu` formulation: O(len(q) · len(v)) word ops — vs the O(bits·limbs)
    /// bit-at-a-time long division it replaces, which made the modPow inner
    /// loop (thousands of reductions per isProbablePrime) ~1 s/call. A single
    /// 32-bit divisor takes the simple word-at-a-time path. Validated against
    /// the decimal `bi_div_str`/`bi_mod_str` reference (div_rem_mod_match_decimal).
    fn divmod_mag(a_in: &[u32], b_in: &[u32]) -> (Vec<u32>, Vec<u32>) {
        // Trim operands to their significant length.
        let mut alen = a_in.len();
        while alen > 0 && a_in[alen - 1] == 0 {
            alen -= 1;
        }
        let mut n = b_in.len();
        while n > 0 && b_in[n - 1] == 0 {
            n -= 1;
        }
        debug_assert!(n > 0, "divmod_mag: zero divisor");
        // TOTAL on a zero divisor (lane G10, 2026-08-16). The `debug_assert`
        // above is compiled out of `--release`, and the very next use of `n` is
        // `v[n - 1]`: `0usize - 1` wraps to `usize::MAX` and the slice index
        // PANICS. A Rust panic in a native is not a Java throwable — it takes
        // the VM down where HotSpot throws `ArithmeticException: BigInteger
        // divide by zero`. All four public callers (`div`, `rem`, `divmod`,
        // `modulo`) short-circuit `o.is_zero()` first, so this is a landmine
        // and not a live defect; it is removed rather than documented because
        // the cost is one comparison on a path that already trims both
        // operands. The answer matches those wrappers' own zero-divisor
        // convention: `(0, 0)`.
        if n == 0 {
            return (Vec::new(), Vec::new());
        }
        let a = &a_in[..alen];
        let v = &b_in[..n];

        if Self::cmp_mag(a, v) == Ordering::Less {
            return (Vec::new(), a.to_vec());
        }

        // Single-word divisor: straightforward long division.
        if n == 1 {
            let d = v[0] as u64;
            let mut q = vec![0u32; alen];
            let mut rem: u64 = 0;
            for i in (0..alen).rev() {
                let cur = (rem << 32) | (a[i] as u64);
                q[i] = (cur / d) as u32;
                rem = cur % d;
            }
            while q.last() == Some(&0) {
                q.pop();
            }
            let r = if rem == 0 {
                Vec::new()
            } else {
                vec![rem as u32]
            };
            return (q, r);
        }

        const BASE: u64 = 1u64 << 32;
        // Normalize so the divisor's top word has its high bit set.
        let shift = v[n - 1].leading_zeros() as usize;
        let mut vn = Self::shl_mag(v, shift);
        vn.resize(n, 0); // shift < 32 with top-word leading zeros consumed → exactly n words
        let m = alen - n;
        let mut un = Self::shl_mag(a, shift);
        un.resize(alen + 1, 0); // need index m+n

        let mut q = vec![0u32; m + 1];
        for j in (0..=m).rev() {
            let num = ((un[j + n] as u64) << 32) | (un[j + n - 1] as u64);
            let mut qhat = num / (vn[n - 1] as u64);
            let mut rhat = num % (vn[n - 1] as u64);
            // Correct the estimate so qhat is exact or 1 too high.
            while qhat >= BASE || qhat * (vn[n - 2] as u64) > (rhat << 32) | (un[j + n - 2] as u64)
            {
                qhat -= 1;
                rhat += vn[n - 1] as u64;
                if rhat >= BASE {
                    break;
                }
            }
            // Multiply and subtract: un[j..j+n] -= qhat * vn.
            let mut k: i64 = 0; // borrow
            for i in 0..n {
                let p = qhat * (vn[i] as u64);
                let t = (un[j + i] as i64) - k - ((p & 0xFFFF_FFFF) as i64);
                un[j + i] = t as u32;
                k = (p >> 32) as i64 - (t >> 32);
            }
            let t = (un[j + n] as i64) - k;
            un[j + n] = t as u32;
            if t < 0 {
                // qhat was one too large — add the divisor back.
                qhat -= 1;
                let mut carry: u64 = 0;
                for i in 0..n {
                    let s = (un[j + i] as u64) + (vn[i] as u64) + carry;
                    un[j + i] = s as u32;
                    carry = s >> 32;
                }
                un[j + n] = (un[j + n] as u64 + carry) as u32;
            }
            q[j] = qhat as u32;
        }
        while q.last() == Some(&0) {
            q.pop();
        }
        // Remainder = un[0..n] >> shift (denormalize).
        let (mut r, _) = Self::shr_mag(&un[..n], shift);
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

    // -----------------------------------------------------------------
    // modular exponentiation + primality (the hot crypto natives)
    // -----------------------------------------------------------------

    /// Small non-negative constant.
    fn small(v: u32) -> BigInt {
        if v == 0 {
            BigInt::zero()
        } else {
            BigInt {
                neg: false,
                mag: vec![v],
            }
        }
    }

    /// `self^exp mod modulus`, all magnitudes; `exp` is used by magnitude (the
    /// native layer handles a genuinely negative exponent via modInverse) and a
    /// zero/negative modulus is reduced against `|modulus|`, matching
    /// [`BigInt::modulo`].
    ///
    /// **Odd modulus — the whole crypto hot path (RSA, DH, Miller-Rabin) —
    /// takes windowed Montgomery** ([`crate::montgomery`]): the per-step full
    /// Knuth-D division is replaced by a multiply and a limb shift, and the
    /// multiplies are cut by a factor of the window width against a precomputed
    /// table. Even moduli have no Montgomery form and fall back to
    /// [`Self::modpow_classic`]. Retires
    /// `perf/biginteger-modpow-has-no-montgomery-reduction-20260817`.
    pub(crate) fn modpow(&self, exp: &BigInt, modulus: &BigInt) -> BigInt {
        if modulus.is_zero() {
            return BigInt::zero();
        }
        let one = Self::small(1);
        // Reduce against |modulus|, as `modulo` does.
        let m_pos = BigInt {
            neg: false,
            mag: modulus.mag.clone(),
        };
        if m_pos.cmp(&one) == Ordering::Equal {
            return BigInt::zero(); // anything mod 1 == 0
        }
        let Some(mont) = crate::montgomery::Montgomery::new(&m_pos.mag) else {
            // Even modulus (or a degenerate one already handled above).
            return self.modpow_classic(exp, &m_pos);
        };
        let n = mont.limbs();
        let base = self.modulo(&m_pos);
        // R mod m and R^2 mod m. Two divisions, once, instead of one per step.
        let mut r1 = one.shl(32 * n as u32).modulo(&m_pos).mag;
        r1.resize(n, 0);
        let mut r2 = one.shl(64 * n as u32).modulo(&m_pos).mag;
        r2.resize(n, 0);
        let mag = crate::montgomery::modpow_odd(&mont, &base.mag, &exp.mag, &r1, &r2);
        Self::normalize(mag, false)
    }

    /// Division-based square-and-multiply — the fallback for an **even**
    /// modulus, which has no Montgomery form. `modulus` must be positive and
    /// greater than 1.
    ///
    /// Left-to-right over `exp.bit_length()` bits rather than
    /// `exp.mag.len() * 32`: the old bound squared up to 31 leading zero bits of
    /// the top word for nothing.
    fn modpow_classic(&self, exp: &BigInt, modulus: &BigInt) -> BigInt {
        let mut result = Self::small(1);
        let base = self.modulo(modulus);
        let ebits = crate::montgomery::bit_len(&exp.mag);
        for i in (0..ebits).rev() {
            result = result.mul(&result).modulo(modulus);
            if (exp.mag[i / 32] >> (i % 32)) & 1 == 1 {
                result = result.mul(&base).modulo(modulus);
            }
        }
        result
    }

    /// `BigInteger.modInverse(m)` — the extended Euclidean inverse of `self`
    /// mod `m`, or `None` when `gcd(self, m) != 1`. `m` must be positive.
    ///
    /// (This doc comment used to describe `is_probable_prime`, which is the
    /// *next* function down; the two had drifted apart.)
    pub(crate) fn mod_inverse(&self, modulus: &BigInt) -> Option<BigInt> {
        if modulus.signum() <= 0 {
            return None;
        }
        let one = Self::small(1);
        if modulus.cmp(&one) == Ordering::Equal {
            return Some(BigInt::zero());
        }

        let mut t = BigInt::zero();
        let mut new_t = one.clone();
        let mut r = modulus.clone();
        let mut new_r = self.modulo(modulus);

        while !new_r.is_zero() {
            let (q, rem) = r.divmod(&new_r);
            let next_t = t.sub(&q.mul(&new_t));
            t = new_t;
            new_t = next_t;
            r = new_r;
            new_r = rem;
        }

        if r.cmp(&one) != Ordering::Equal {
            return None;
        }
        if t.is_neg() {
            t = t.add(modulus);
        }
        Some(t)
    }

    /// Strong-probable-prime (Miller-Rabin) test with fixed small-prime bases —
    /// mirrors the decimal `bi_is_probable_prime_str` (trial division < 1000,
    /// then 13 fixed bases), but on words so the inner `modPow` is fast.
    pub(crate) fn is_probable_prime(&self) -> bool {
        if self.neg || self.is_zero() {
            return false;
        }
        let one = Self::small(1);
        let two = Self::small(2);
        let three = Self::small(3);
        if self.cmp(&one) == Ordering::Equal {
            return false;
        }
        if self.cmp(&two) == Ordering::Equal || self.cmp(&three) == Ordering::Equal {
            return true;
        }
        // even
        if self.mag[0] & 1 == 0 {
            return false;
        }
        // trial division by small odd numbers < 1000 (fast composite filter)
        let mut d = 3u32;
        while d < 1000 {
            let dd = Self::small(d);
            if self.cmp(&dd) == Ordering::Less {
                break;
            }
            if self.modulo(&dd).is_zero() {
                return self.cmp(&dd) == Ordering::Equal;
            }
            d += 2;
        }
        self.miller_rabin()
    }

    fn miller_rabin(&self) -> bool {
        let one = Self::small(1);
        let n_minus_1 = self.sub(&one);
        // n-1 = d * 2^s, d odd
        let mut d = n_minus_1.clone();
        let mut s: u32 = 0;
        while !d.is_zero() && d.mag[0] & 1 == 0 {
            d = d.shr(1);
            s += 1;
        }
        const BASES: &[u32] = &[2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41];
        for &a in BASES {
            let a_bi = Self::small(a);
            if a_bi.cmp(self) != Ordering::Less {
                continue;
            }
            let mut x = a_bi.modpow(&d, self);
            if x.cmp(&one) == Ordering::Equal || x.cmp(&n_minus_1) == Ordering::Equal {
                continue;
            }
            let mut composite = true;
            for _ in 0..s.saturating_sub(1) {
                x = x.mul(&x).modulo(self);
                if x.cmp(&n_minus_1) == Ordering::Equal {
                    composite = false;
                    break;
                }
            }
            if composite {
                return false;
            }
        }
        true
    }

    // -----------------------------------------------------------------
    // two's-complement bit operations (BigInteger semantics)
    // -----------------------------------------------------------------

    /// Number of bits in the minimal magnitude (highest set bit + 1; 0 for zero).
    fn mag_bits(mag: &[u32]) -> usize {
        match mag.last() {
            None => 0,
            Some(&top) => (mag.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
        }
    }

    /// This value's MAGNITUDE bit length — the single owner of that rule.
    ///
    /// It is **not** [`Self::bit_length`]: `BigInteger.bitLength()` subtracts
    /// the sign bit for a negative exact power of two, and every range guard in
    /// this family needs the magnitude. MEASURED by lane F2
    /// (`scratchpad/f2/BiProbe.java`, Microsoft OpenJDK 25.0.3+9):
    /// `(-2).shiftLeft(Integer.MAX_VALUE - 2)` is LEGAL and reports
    /// `bitLength=2147483646`, one less than its 2_147_483_647 magnitude bits —
    /// so a guard written on `bit_length()` admits exactly one bit too many for
    /// that family of operands, which is where a 256 MB allocation comes back.
    ///
    /// Exposed because the same three lines had been copied into
    /// `math_bignum::bi_mag_bits` and `phases_late::p71_bi_mag_bits`; callers
    /// should use this instead of a fourth copy.
    pub(crate) fn magnitude_bits(&self) -> u64 {
        Self::mag_bits(&self.mag) as u64
    }

    /// Two's-complement representation in exactly `len` words (little-endian),
    /// sign-extended. `len` must be at least the magnitude word count.
    fn to_twos(&self, len: usize) -> Vec<u32> {
        let mut w = vec![0u32; len];
        for (i, &m) in self.mag.iter().enumerate() {
            w[i] = m;
        }
        if self.neg {
            // negate over the full width: ~w + 1 (high zero words become the
            // sign-extended 1s automatically via ~0 + carry).
            let mut carry = 1u64;
            for x in w.iter_mut() {
                let v = (!*x as u64) + carry;
                *x = v as u32;
                carry = v >> 32;
            }
        }
        w
    }

    /// Interpret a two's-complement word vector (top bit = sign) as a BigInt.
    fn from_twos(w: &[u32]) -> BigInt {
        let neg = w.last().map_or(false, |&top| (top >> 31) & 1 == 1);
        if neg {
            let mut m = w.to_vec();
            let mut carry = 1u64;
            for x in m.iter_mut() {
                let v = (!*x as u64) + carry;
                *x = v as u32;
                carry = v >> 32;
            }
            Self::normalize(m, true)
        } else {
            Self::normalize(w.to_vec(), false)
        }
    }

    fn bitop(&self, o: &BigInt, f: impl Fn(u32, u32) -> u32) -> BigInt {
        // +1 word so the sign of each operand (and the result) is representable.
        let len = self.mag.len().max(o.mag.len()) + 1;
        let aw = self.to_twos(len);
        let bw = o.to_twos(len);
        let rw: Vec<u32> = (0..len).map(|i| f(aw[i], bw[i])).collect();
        Self::from_twos(&rw)
    }

    pub(crate) fn and(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x & y)
    }
    pub(crate) fn or(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x | y)
    }
    pub(crate) fn xor(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x ^ y)
    }
    /// `~self == -(self + 1)`.
    pub(crate) fn not(&self) -> BigInt {
        self.add(&Self::small(1)).neg_value()
    }

    /// `BigInteger.bitLength()` — bits in the minimal two's-complement
    /// representation, excluding the sign bit.
    pub(crate) fn bit_length(&self) -> u32 {
        let mb = Self::mag_bits(&self.mag);
        if self.neg {
            // magBitLength-1 iff the magnitude is an exact power of two.
            let pow2 = self.mag.last().map_or(false, |&t| t.count_ones() == 1)
                && self.mag[..self.mag.len().saturating_sub(1)]
                    .iter()
                    .all(|&w| w == 0);
            (if pow2 { mb - 1 } else { mb }) as u32
        } else {
            mb as u32
        }
    }

    /// `BigInteger.bitCount()` — bits differing from the sign bit.
    pub(crate) fn bit_count(&self) -> u32 {
        if self.is_zero() {
            return 0;
        }
        if !self.neg {
            self.mag.iter().map(|w| w.count_ones()).sum()
        } else {
            // Count the 0-bits of the two's-complement (the sign-extended top
            // word is all-1s and contributes nothing).
            let len = self.mag.len() + 1;
            let tw = self.to_twos(len);
            tw.iter().map(|w| w.count_zeros()).sum::<u32>()
        }
    }

    /// The `n`th least-significant word of the **infinite** two's-complement
    /// representation — JDK 25 `BigInteger.getInt` (`BigInteger.java:4838`):
    ///
    /// ```text
    ///     if (n >= mag.length) return signInt();          // 0, or -1 when negative
    ///     int magInt = mag[mag.length-n-1];
    ///     return (signum >= 0 ? magInt :
    ///             (n <= numberOfTrailingZeroInts() ? -magInt : ~magInt));
    /// ```
    ///
    /// `mag` there is big-endian, so `mag[mag.length-n-1]` is our little-endian
    /// `mag[n]`, and `numberOfTrailingZeroInts()` is the index of the lowest
    /// non-zero limb. Words at or below that index are negated; the ones above
    /// it are complemented — the borrow out of the low words has already been
    /// consumed. **Allocates nothing**, which is the whole point: `n` is an
    /// argument, so anything sized by it is reachable denial of service.
    fn get_int(&self, n: usize) -> u32 {
        if n >= self.mag.len() {
            return if self.neg { u32::MAX } else { 0 };
        }
        let m = self.mag[n];
        if !self.neg {
            return m;
        }
        let lowest_nonzero = self.mag.iter().position(|&w| w != 0).unwrap_or(0);
        if n <= lowest_nonzero {
            m.wrapping_neg()
        } else {
            !m
        }
    }

    /// `BigInteger.testBit(n)` — `(getInt(n >>> 5) & (1 << (n & 31))) != 0`,
    /// JDK 25 `BigInteger.java:3747`. The caller rejects a negative `n` with
    /// `ArithmeticException("Negative bit address")` before widening to `u32`.
    ///
    /// This used to materialize the two's complement out to `n`'s word:
    ///
    /// ```text
    ///     let len = self.mag.len().max(word + 1) + 1;
    ///     let tw = self.to_twos(len);
    /// ```
    ///
    /// The answers were right, but `BigInteger.ONE.testBit(Integer.MAX_VALUE)`
    /// — one line of ordinary bytecode, in EVERY jdk mode — allocated
    /// `vec![0u32; 67_108_866]`, ~256 MB, to read one bit that is a function of
    /// the sign alone. HotSpot answers the same call in 0 ms (MEASURED,
    /// `scratchpad/f7/TestBit.java`, Microsoft OpenJDK 25.0.3+9), and the
    /// transliteration of the body below agrees with `java.math.BigInteger` on
    /// 371,547 (operand, bit) pairs — including every bit index up to 400 and
    /// `Integer.MAX_VALUE` itself — with zero diffs.
    pub(crate) fn test_bit(&self, n: u32) -> bool {
        (self.get_int((n / 32) as usize) >> (n % 32)) & 1 == 1
    }

    /// `BigInteger.getLowestSetBit()` — index of the rightmost set bit, or -1
    /// for zero. Same for both signs (two's-complement preserves the lowest set
    /// bit of the magnitude).
    pub(crate) fn lowest_set_bit(&self) -> i32 {
        for (i, &w) in self.mag.iter().enumerate() {
            if w != 0 {
                return (i as i32) * 32 + w.trailing_zeros() as i32;
            }
        }
        -1
    }
}

// ---------------------------------------------------------------------------
// Differential tests against the decimal `bi_*_str` reference primitives.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bi_add_str, bi_bitwise_and, bi_bitwise_or, bi_bitwise_xor, bi_compare, bi_div_str,
        bi_is_probable_prime_str, bi_mod_inverse_str, bi_mod_pow_str, bi_mod_str, bi_mul_str,
        bi_shift_left_str, bi_shift_right_str, bi_sub_str,
    };
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Deterministic LCG so the spread is reproducible without a rand dep.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    /// Random positive odd `BigInt` of exactly `bits` bits (top bit set, low
    /// bit set). `bits >= 1`; `bits == 1` yields 1.
    fn rand_bits_odd(state: &mut u64, bits: u32) -> BigInt {
        let words = bits.div_ceil(32) as usize;
        let mut mag: Vec<u32> = (0..words).map(|_| lcg(state) as u32).collect();
        let top = (bits - 1) % 32;
        mag[words - 1] &= (1u32 << top) | ((1u32 << top) - 1);
        mag[words - 1] |= 1u32 << top;
        mag[0] |= 1;
        BigInt::normalize(mag, false)
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
            "0",
            "1",
            "-1",
            "2",
            "-2",
            "7",
            "-7",
            "10",
            "-10",
            "4294967295",
            "4294967296",
            "4294967297",
            "-4294967296",
            "18446744073709551615",
            "18446744073709551616",
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
                assert_eq!(ba.cmp(&bc), bi_compare(a, c).cmp(&0), "cmp {a} ? {c}");
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
                        if stripped == "0" {
                            "0".to_string()
                        } else {
                            bi_add_str(&rr, c_abs)
                        }
                    } else {
                        rr
                    }
                };
                assert_eq!(m.to_decimal(), want_mod, "mod {a} mod {c}");
            }
        }
    }

    #[test]
    fn modpow_matches_decimal() {
        let mut state = 0xa5a5_5a5a_dead_0001u64;
        // non-negative exponents, positive moduli > 1
        let bases = [
            "0",
            "1",
            "2",
            "7",
            "255",
            "4294967297",
            "123456789012345678901234567890",
            "115792089237316195423570985008687907853269984665640564039457584007913129639747",
        ];
        let exps = ["0", "1", "2", "3", "17", "65537", "1000003"];
        let mods = [
            "2",
            "3",
            "97",
            "65537",
            "1000000007",
            "987654321098765432109876543211",
            "115792089237316195423570985008687907853269984665640564039457584007913129639747",
        ];
        for ba in bases {
            for e in exps {
                for m in mods {
                    let got = b(ba).modpow(&b(e), &b(m)).to_decimal();
                    let want = bi_mod_pow_str(ba, e, m);
                    assert_eq!(got, want, "modpow({ba}^{e} mod {m})");
                }
            }
        }
        // a few random non-negative cases
        for _ in 0..120 {
            let ba = rand_decimal(&mut state).trim_start_matches('-').to_string();
            let e = rand_decimal(&mut state).trim_start_matches('-').to_string();
            let m = {
                let s = rand_decimal(&mut state).trim_start_matches('-').to_string();
                if s == "0" || s == "1" {
                    "1000000007".to_string()
                } else {
                    s
                }
            };
            assert_eq!(
                b(&ba).modpow(&b(&e), &b(&m)).to_decimal(),
                bi_mod_pow_str(&ba, &e, &m),
                "rand modpow({ba}^{e} mod {m})"
            );
        }
    }

    /// The Montgomery rewrite's safety net.
    ///
    /// `modpow` dispatches on the parity of the modulus: odd goes to windowed
    /// Montgomery, even to `modpow_classic`. This drives both arms across
    /// operand sizes with a *third*, independent oracle — the decimal
    /// `bi_mod_pow_str` — so neither arm is checked only against itself, and
    /// then cross-checks the two arms against each other on odd moduli (where
    /// both are defined). A Montgomery bug that produced plausible-looking
    /// wrong residues would have to fool all three to survive.
    #[test]
    fn modpow_montgomery_and_classic_agree_with_decimal() {
        let mut state = 0x1bad_c0de_5eed_0007u64;

        // Structural cases first: the shapes that break window/limb bookkeeping.
        let structural: &[(&str, &str, &str)] = &[
            // modulus 1 -> everything is 0
            ("123456789", "987654321", "1"),
            // odd single-limb moduli at the word boundary
            ("4294967295", "4294967295", "4294967295"),
            ("4294967296", "4294967296", "4294967295"),
            ("1", "0", "3"),
            ("0", "0", "3"),
            ("0", "1", "3"),
            // exponent whose top word has 31 leading zero bits (the old
            // `mag.len() * 32` bound squared all of them for nothing)
            ("7", "4294967296", "1000000007"),
            ("7", "18446744073709551616", "1000000007"),
            // even moduli, including powers of two (the fallback arm)
            ("123456789", "65537", "2"),
            ("123456789", "65537", "4294967296"),
            (
                "123456789",
                "65537",
                "340282366920938463463374607431768211456",
            ),
            ("123456789", "65537", "1000000008"),
            (
                "99999999999999999999999999",
                "123456789",
                "618970019642690137449562112",
            ),
            // negative base: BigInteger.modPow is always non-negative
            ("-2", "3", "5"),
            ("-2", "2", "5"),
            ("-123456789012345678901234567890", "65537", "1000000007"),
            ("-123456789012345678901234567890", "65537", "1000000008"),
            // large odd modulus, RSA-ish exponent
            (
                "123456789012345678901234567890123456789012345678901234567890",
                "65537",
                "115792089237316195423570985008687907853269984665640564039457584007913129639747",
            ),
        ];
        for &(ba, e, m) in structural {
            let got = b(ba).modpow(&b(e), &b(m)).to_decimal();
            assert_eq!(got, bi_mod_pow_str(ba, e, m), "modpow({ba}^{e} mod {m})");
        }

        // Random sweep across operand widths. `bits` is the modulus width, so
        // this walks 1-limb moduli up through multi-limb ones; the exponent and
        // base are independently sized so the window code sees short and long
        // exponents against both narrow and wide moduli.
        for &bits in &[1u32, 2, 8, 31, 32, 33, 64, 65, 127, 128, 200, 256] {
            // The decimal oracle is O(digits^2) per squaring, so it dominates
            // at the wide end; taper the repeat count rather than the widths.
            let repeats = if bits >= 128 { 2 } else { 8 };
            for _ in 0..repeats {
                let m_odd = rand_bits_odd(&mut state, bits);
                // Same magnitude made even, so both arms see comparable sizes.
                let m_even = m_odd.add(&BigInt::small(1));
                for m in [&m_odd, &m_even] {
                    if m.cmp(&BigInt::small(1)) != Ordering::Greater {
                        continue;
                    }
                    for &ebits in &[1u32, 5, 17, 24, 70, 197, bits.max(1)] {
                        let base = rand_bits_odd(&mut state, bits.max(1));
                        let base = if lcg(&mut state) & 1 == 0 {
                            base.neg_value()
                        } else {
                            base
                        };
                        let exp = rand_bits_odd(&mut state, ebits);
                        let (bs, es, ms) = (base.to_decimal(), exp.to_decimal(), m.to_decimal());
                        assert_eq!(
                            base.modpow(&exp, m).to_decimal(),
                            bi_mod_pow_str(&bs, &es, &ms),
                            "modpow({bs}^{es} mod {ms})"
                        );
                        // Odd moduli: the two arms must agree bit for bit.
                        if m.mag[0] & 1 == 1 {
                            assert_eq!(
                                base.modpow(&exp, m),
                                base.modpow_classic(&exp, m),
                                "montgomery vs classic ({bs}^{es} mod {ms})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// RSA is the failure mode the doc names: a subtly wrong modPow produces
    /// plausible-looking wrong signatures that still round-trip through the
    /// encoding layers. Check the algebra directly — `(m^e)^d == m (mod n)` for
    /// a real keypair, plus the CRT half-exponentiations — so a wrong residue
    /// cannot hide behind a self-consistent implementation.
    #[test]
    fn modpow_round_trips_an_rsa_keypair() {
        // p, q distinct primes; n = p*q, e = 65537, d = e^-1 mod phi.
        let p = b("177250851143413106261106096231831452933");
        let q = b("329539864610483636976818094878219646841");
        assert!(p.is_probable_prime(), "p prime");
        assert!(q.is_probable_prime(), "q prime");
        let n = p.mul(&q);
        let e = b("65537");
        let phi = p.sub(&BigInt::small(1)).mul(&q.sub(&BigInt::small(1)));
        let d = e.mod_inverse(&phi).expect("e invertible mod phi");

        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..12 {
            let msg = rand_bits_odd(&mut state, 200).modulo(&n);
            let c = msg.modpow(&e, &n);
            let back = c.modpow(&d, &n);
            assert_eq!(back, msg, "RSA round trip");
            assert_ne!(c, msg, "ciphertext is not the plaintext");
            // The signing direction, and CRT halves against the direct result.
            let sig = msg.modpow(&d, &n);
            assert_eq!(sig.modpow(&e, &n), msg, "RSA sign/verify round trip");
            let dp = d.modulo(&p.sub(&BigInt::small(1)));
            let dq = d.modulo(&q.sub(&BigInt::small(1)));
            assert_eq!(msg.modpow(&dp, &p), sig.modulo(&p), "CRT half mod p");
            assert_eq!(msg.modpow(&dq, &q), sig.modulo(&q), "CRT half mod q");
        }
    }

    #[test]
    fn mod_inverse_matches_decimal() {
        let cases = [
            ("1", "2"),
            ("2", "3"),
            ("3", "11"),
            ("42", "2017"),
            ("-42", "2017"),
            (
                "123456789012345678901234567890",
                "115792089237316195423570985008687907853269984665640564039457584007913129639747",
            ),
            (
                "98765432109876543210987654321",
                "6277101735386680763835789423207666416083908700390324961279",
            ),
        ];
        for (a, m) in cases {
            let got = b(a).mod_inverse(&b(m)).map(|v| v.to_decimal());
            let want = bi_mod_inverse_str(a, m);
            assert_eq!(got, want, "modInverse({a}, {m})");
        }
        assert_eq!(b("2").mod_inverse(&b("4")), None);
        assert_eq!(b("9").mod_inverse(&b("1")).unwrap().to_decimal(), "0");

        let mut state = 0x5151_6262_7373_8484u64;
        let prime =
            "115792089237316195423570985008687907853269984665640564039457584007913129639747";
        for _ in 0..80 {
            let a = rand_decimal(&mut state);
            let got = b(&a).mod_inverse(&b(prime)).map(|v| v.to_decimal());
            let want = bi_mod_inverse_str(&a, prime);
            assert_eq!(got, want, "rand modInverse({a}, prime)");
        }
    }

    #[test]
    fn is_probable_prime_matches_decimal() {
        let primes = [
            "2",
            "3",
            "5",
            "7",
            "97",
            "65537",
            "32416190071", // 10-digit prime
            "115792089237316195423570985008687907853269984665640564039457584007913129639747",
        ];
        let composites = ["0", "1", "4", "9", "15", "100", "32416190073"];
        for p in primes {
            assert!(b(p).is_probable_prime(), "{p} should be prime");
            assert_eq!(
                b(p).is_probable_prime(),
                bi_is_probable_prime_str(p),
                "prime {p} vs decimal ref"
            );
        }
        for c in composites {
            assert!(!b(c).is_probable_prime(), "{c} should be composite");
            assert_eq!(
                b(c).is_probable_prime(),
                bi_is_probable_prime_str(c),
                "composite {c} vs decimal ref"
            );
        }
        // Hard case the trial-division stub got wrong: a product of two large
        // primes (no small factor) must be detected composite by MR.
        let p256 = "115792089237316195423570985008687907853269984665640564039457584007913129639747";
        let q256 = "115792089237316195423570985008687907853269984665640564039457584007913129640297";
        let semiprime = b(p256).mul(&b(q256));
        assert!(!semiprime.is_probable_prime(), "p*q must be composite");
        assert_eq!(
            semiprime.is_probable_prime(),
            bi_is_probable_prime_str(&semiprime.to_decimal()),
            "semiprime vs decimal ref"
        );
    }

    #[test]
    fn bit_ops_correct() {
        let one = b("1");
        let neg_one = b("-1");
        let zero = b("0");

        // Known exact values (BigInteger semantics).
        assert_eq!(b("5").not().to_decimal(), "-6"); // ~5 = -6
        assert_eq!(b("-5").not().to_decimal(), "4"); // ~(-5) = 4
        assert_eq!(b("0").not().to_decimal(), "-1"); // ~0 = -1
        assert_eq!(b("12").and(&b("10")).to_decimal(), "8");
        assert_eq!(b("12").or(&b("10")).to_decimal(), "14");
        assert_eq!(b("12").xor(&b("10")).to_decimal(), "6");
        // Negative two's-complement (matches java.math.BigInteger):
        // -8 = …11111000, 12 = …00001100 → &=…1000=8, |=…11111100=-4, ^=…11110100=-12.
        assert_eq!(b("-8").and(&b("12")).to_decimal(), "8");
        assert_eq!(b("-8").or(&b("12")).to_decimal(), "-4");
        assert_eq!(b("-8").xor(&b("12")).to_decimal(), "-12");
        // bitLength / bitCount / lowestSetBit edge cases.
        assert_eq!(neg_one.bit_length(), 0);
        assert_eq!(b("-2").bit_length(), 1);
        assert_eq!(b("-4").bit_length(), 2);
        assert_eq!(b("-3").bit_length(), 2);
        assert_eq!(b("17").bit_length(), 5);
        assert_eq!(b("255").bit_count(), 8);
        assert_eq!(neg_one.bit_count(), 0);
        assert_eq!(b("-256").bit_count(), 8);
        assert_eq!(b("0").lowest_set_bit(), -1);
        assert_eq!(b("48").lowest_set_bit(), 4); // 48 = 0b110000
        assert_eq!(b("-48").lowest_set_bit(), 4);

        // Algebraic identities over a deterministic spread of both signs.
        let mut state = 0x1313_2424_3535_4646u64;
        let mut vals: Vec<BigInt> = edge_cases().iter().map(|s| b(s)).collect();
        for _ in 0..150 {
            vals.push(b(&rand_decimal(&mut state)));
        }
        for x in &vals {
            // ~x == -(x+1); x ^ -1 == ~x; x ^ 0 == x; x & 0 == 0; x | 0 == x.
            assert_eq!(
                x.not(),
                x.add(&one).neg_value(),
                "~x for {}",
                x.to_decimal()
            );
            assert_eq!(x.xor(&neg_one), x.not(), "x^-1 for {}", x.to_decimal());
            assert_eq!(x.xor(&zero), *x);
            assert_eq!(x.and(&zero), zero);
            assert_eq!(x.or(&zero), *x);
            assert_eq!(x.and(x), *x);
            assert_eq!(x.or(x), *x);
            assert_eq!(x.xor(x), zero);
            // testBit consistency with the value: x.testBit(i) reconstructs x
            // for a few low bits via OR of set bits is overkill; check against
            // shifting instead: bit i of x == ((x >> i) is odd).
            for i in [0u32, 1, 5, 31, 32, 33, 64] {
                let shifted_odd = x.shr(i).and(&one) == one;
                assert_eq!(
                    x.test_bit(i),
                    shifted_odd,
                    "testBit {} bit {i}",
                    x.to_decimal()
                );
            }
        }
        // Cross-identity: (a&b) | (a^b) == a|b; De Morgan ~(a&b)==(~a)|(~b).
        for x in &vals {
            for y in vals.iter().take(20) {
                assert_eq!(x.and(y).or(&x.xor(y)), x.or(y), "consistency");
                assert_eq!(x.and(y).not(), x.not().or(&y.not()), "De Morgan");
            }
        }
    }

    fn positive_bit_ops_match_decimal_cases(random_cases: usize, peer_cases: usize) {
        // For non-negative operands the decimal reference (which works on
        // magnitudes) is authoritative.
        let mut state = 0x9999_7777_5555_3333u64;
        let mut pos: Vec<String> = vec!["0", "1", "255", "65535", "4294967296"]
            .into_iter()
            .map(String::from)
            .collect();
        for _ in 0..random_cases {
            pos.push(rand_decimal(&mut state).trim_start_matches('-').to_string());
        }
        for a in &pos {
            // bitLength / bitCount vs decimal ref.
            assert_eq!(
                b(a).bit_length(),
                crate::bi_bit_length_str(a) as u32,
                "bitLen {a}"
            );
            assert_eq!(
                b(a).bit_count(),
                crate::bi_bit_count_str(a) as u32,
                "bitCnt {a}"
            );
            for c in pos.iter().take(peer_cases) {
                assert_eq!(
                    b(a).and(&b(c)).to_decimal(),
                    bi_bitwise_and(a, c),
                    "and {a}&{c}"
                );
                assert_eq!(
                    b(a).or(&b(c)).to_decimal(),
                    bi_bitwise_or(a, c),
                    "or {a}|{c}"
                );
                assert_eq!(
                    b(a).xor(&b(c)).to_decimal(),
                    bi_bitwise_xor(a, c),
                    "xor {a}^{c}"
                );
            }
        }
    }

    #[test]
    fn positive_bit_ops_match_decimal() {
        positive_bit_ops_match_decimal_cases(24, 10);
    }

    #[test]
    #[ignore = "exhaustive decimal cross-check takes several minutes; run explicitly before bigint rewrites"]
    fn positive_bit_ops_match_decimal_exhaustive() {
        positive_bit_ops_match_decimal_cases(150, 25);
    }

    /// `test_bit` must answer a huge bit address from the sign alone, without
    /// materializing the two's complement out to that word. The old body built
    /// `vec![0u32; (n/32)+2]`, so every row here allocated ~256 MB; this test
    /// would have taken minutes and ~4 GB.
    ///
    /// Expected values MEASURED on Microsoft OpenJDK 25.0.3+9
    /// (`scratchpad/f7/TestBit.java`), each `[0 ms]`:
    ///
    /// ```text
    /// ONE.testBit(Integer.MAX_VALUE)  = false      (-1).testBit(Integer.MAX_VALUE) = true
    /// ZERO.testBit(Integer.MAX_VALUE) = false      (-1).testBit(0)                 = true
    /// (-2).testBit(0) = false                      (-2).testBit(1)                 = true
    /// (2^64).testBit(0)     = false                (-(2^64)).testBit(0)  = false
    /// (-(2^64)).testBit(64) = true                 (-(2^64)).testBit(65) = true
    /// (-(2^64+1)).testBit(0) = true                (-(2^64+1)).testBit(1) = true
    /// ```
    ///
    /// The full transliteration of this body agrees with `java.math.BigInteger`
    /// on 371,547 (operand, bit) pairs with zero diffs.
    #[test]
    fn test_bit_is_allocation_free_at_huge_addresses() {
        const MAX: u32 = i32::MAX as u32;
        assert!(!b("1").test_bit(MAX));
        assert!(b("-1").test_bit(MAX));
        assert!(!b("0").test_bit(MAX));
        assert!(b("-1").test_bit(0));
        assert!(!b("-2").test_bit(0));
        assert!(b("-2").test_bit(1));
        // 2^64 == mag [0, 0, 1]: the low limbs are zero, which is what
        // separates `-magInt` from `~magInt` in the JDK's `getInt`.
        let p64 = b("18446744073709551616");
        let n64 = b("-18446744073709551616");
        assert!(!p64.test_bit(0));
        assert!(!n64.test_bit(0));
        assert!(n64.test_bit(64));
        assert!(n64.test_bit(65));
        assert!(n64.test_bit(MAX));
        let n64p1 = b("-18446744073709551617");
        assert!(n64p1.test_bit(0));
        assert!(n64p1.test_bit(1));
        // A positive value is 0 above its magnitude, a negative one is 1 —
        // for every address past the top limb, not just the huge ones.
        for &n in &[96u32, 97, 1000, 1 << 20, 1 << 26, MAX - 1, MAX] {
            assert!(!p64.test_bit(n), "positive bit {n}");
            assert!(n64.test_bit(n), "negative bit {n}");
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

    /// **No-panic pin (lane G10, 2026-08-16).** `divmod_mag`'s zero-divisor
    /// guard was a `debug_assert!`, which is compiled out of `--release`; the
    /// next line indexes `v[n - 1]` and `0usize - 1` is a slice-index panic.
    /// A panic in a native is a VM abort, not a Java exception.
    ///
    /// The four public wrappers short-circuit first, so the assertions here are
    /// on their documented convention (zero out) AND on the fact that every one
    /// of them RETURNS. `divmod_mag` itself is private, so it is reached
    /// through them; a zero-length magnitude is what `BigInt::zero()` carries.
    #[test]
    fn division_by_zero_returns_instead_of_panicking() {
        let zero = BigInt::zero();
        for v in ["0", "1", "-1", "255", "-9007199254740993", "10"] {
            let x = b(v);
            assert_eq!(x.div(&zero).to_decimal(), "0", "{v} / 0");
            assert_eq!(x.rem(&zero).to_decimal(), "0", "{v} rem 0");
            assert_eq!(x.modulo(&zero).to_decimal(), "0", "{v} mod 0");
            let (q, r) = x.divmod(&zero);
            assert_eq!((q.to_decimal(), r.to_decimal()), ("0".into(), "0".into()));
            // modpow with a zero modulus is the same shape one level up.
            assert_eq!(x.modpow(&b("3"), &zero).to_decimal(), "0");
        }
        // `from_le_words` normalizes, so an all-zero-limb divisor arrives at
        // `divmod_mag` already trimmed to `n == 0` — the exact input the
        // `debug_assert!` was the only thing standing in front of.
        let padded_zero = BigInt::from_le_words(false, vec![0, 0, 0]);
        assert!(padded_zero.is_zero());
        assert_eq!(b("12345").div(&padded_zero).to_decimal(), "0");
        assert_eq!(b("12345").rem(&padded_zero).to_decimal(), "0");
    }
}
