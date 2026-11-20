// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// Limb count at or above which BOTH operands of [`BigInt::mul`] go through
/// Karatsuba instead of the schoolbook kernel (the same 80 `BigInteger` uses;
/// it also keeps 2048-bit operands, 64 limbs, on the schoolbook loop, which
/// allocates no temporaries).
const KARATSUBA_THRESHOLD: usize = 80;

/// `CRATONVM_BIGINT_FAST_MUL` (default ON; `0` restores the schoolbook-only
/// multiply and the unfactored `pow`): subquadratic limb multiplication for
/// the BigInteger/BigDecimal natives. Asked only for operands of at least
/// [`KARATSUBA_THRESHOLD`] limbs (thousands of word products per call), so it
/// is read live rather than cached in a static.
pub fn fast_mul_enabled() -> bool {
    true
}

/// Magnitudes of at least this many limbs (~5 000 digits) are rendered by
/// [`BigInt::to_decimal_dc`]; below it the chunked loop is as fast.
const TO_DECIMAL_DC_MIN_WORDS: usize = 512;
/// Pieces of the divide-and-conquer conversion below this many limbs are
/// rendered by the chunked loop.
const TO_DECIMAL_DC_BASE_WORDS: usize = 128;
/// Reciprocals of divisors up to this many bits are one Knuth division.
const RECIPROCAL_BASE_BITS: u64 = 32 * 128;



/// [`BigInt::from_decimal`] strings (after the sign) longer than this many
/// bytes take the fast parse; shorter ones are at most two limbs of the
/// digit-at-a-time loop, and never read the switch.
const FROM_DECIMAL_CHUNK_MIN_DIGITS: usize = 18;
/// Pieces of the divide-and-conquer parse up to this many digits (~128
/// limbs) are parsed nine digits per step.
const FROM_DECIMAL_DC_BASE_DIGITS: usize = 9 * 128;

/// `CRATONVM_BIGINT_FROM_DECIMAL_FAST` (default ON; `0` restores the
/// digit-at-a-time parse for every length): round 14 wave 2, lane bigdec
/// (BD4-1). Asked only for strings of more than
/// [`FROM_DECIMAL_CHUNK_MIN_DIGITS`] digits, so it is read live, like
/// [`fast_mul_enabled`].
pub fn from_decimal_fast_enabled() -> bool {
    true
}

/// One level of the divide-and-conquer decimal conversion: `d = 10^width`,
/// `width = 9 * 2^k`, its bit length, and its Barrett reciprocal
/// `floor(2^(2 * bits) / d)` once a division at this level needs it.
struct DecimalLevel {
    d: BigInt,
    bits: u64,
    width: usize,
    recip: Option<BigInt>,
}

/// Signed arbitrary-precision integer.
///
/// `mag` is the little-endian base-2^32 magnitude, **normalized**: it has no
/// trailing zero limbs, and the empty vector is the canonical zero. `neg` is
/// always `false` when the value is zero, so equality is structural.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

impl BigInt {
    pub fn zero() -> Self {
        BigInt {
            neg: false,
            mag: Vec::new(),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_neg(&self) -> bool {
        self.neg
    }

    pub fn signum(&self) -> i32 {
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
    pub fn from_le_words(neg: bool, words: Vec<u32>) -> Self {
        Self::normalize(words, neg)
    }

    /// Borrow the normalized little-endian magnitude words.
    pub fn mag_le(&self) -> &[u32] {
        &self.mag
    }

    // -----------------------------------------------------------------
    // decimal boundary (only for String <-> BigInteger + tests)
    // -----------------------------------------------------------------

    /// Parse a decimal string: one optional leading `-` or `+`, and every
    /// other byte that is not an ASCII digit is SKIPPED (callers rely on it:
    /// `"1.5"` reads as 15). Strings of more than
    /// [`FROM_DECIMAL_CHUNK_MIN_DIGITS`] bytes take [`Self::from_decimal_fast`]
    /// (nine digits per step, divide and conquer when wide) unless
    /// `CRATONVM_BIGINT_FROM_DECIMAL_FAST=0`; the rest, and the switched-off
    /// road, the digit-at-a-time loop.
    pub fn from_decimal(s: &str) -> Self {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if digits.len() > FROM_DECIMAL_CHUNK_MIN_DIGITS && from_decimal_fast_enabled() {
            return Self::from_decimal_fast(neg, digits.as_bytes(), FROM_DECIMAL_DC_BASE_DIGITS);
        }
        Self::from_decimal_digitwise(neg, digits)
    }

    /// The digit-at-a-time parse (the reference road): one full-magnitude
    /// multiply-by-ten per digit, O(digits * words).
    fn from_decimal_digitwise(neg: bool, digits: &str) -> Self {
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

    // -----------------------------------------------------------------
    // Fast decimal parse (round 14 wave 2, lane bigdec; proposal BD4-1 of
    // `jit-r13-bigdec4-proposals-RETIRED-20260929.md`)
    // -----------------------------------------------------------------
    //
    // The inverse of `to_decimal_dc` needs no division: split the digit
    // string `9 * 2^k` digits from the right, `x = hi * 10^(9 * 2^k) + lo`,
    // with `10^(9 * 2^k)` built by squaring `10^9` and Karatsuba products;
    // pieces of at most `base_digits` digits are parsed nine digits per step
    // (`x = x * 10^9 + chunk`, one pass over the magnitude per chunk instead
    // of per digit). Pure multiplication and addition: there is no estimate
    // to correct, and every piece is an exact integer.

    /// [`Self::from_decimal`]'s fast arm over the bytes after the sign.
    /// Non-digit bytes are dropped first (the digit-at-a-time loop skips
    /// them), so both roads read the same digit sequence.
    fn from_decimal_fast(neg: bool, bytes: &[u8], base_digits: usize) -> Self {
        let filtered: Vec<u8>;
        let digits: &[u8] = if bytes.iter().all(u8::is_ascii_digit) {
            bytes
        } else {
            filtered = bytes.iter().copied().filter(u8::is_ascii_digit).collect();
            &filtered
        };
        // Leading zeros only cost multiplications by zero-valued pieces.
        let first = digits
            .iter()
            .position(|&b| b != b'0')
            .unwrap_or(digits.len());
        let digits = &digits[first..];
        let mut powers: Vec<BigInt> = Vec::new();
        let x = Self::parse_dc(digits, base_digits.max(9), &mut powers);
        Self::normalize(x.mag, neg)
    }

    /// `digits` (ASCII digits only) as a non-negative value, nine digits per
    /// step: O(digits * words / 9).
    fn parse_chunked(digits: &[u8]) -> BigInt {
        let mut mag: Vec<u32> = Vec::with_capacity(digits.len() / 9 + 2);
        let head = match digits.len() % 9 {
            0 => 9,
            r => r,
        };
        let mut start = 0usize;
        let mut len = head;
        while start < digits.len() {
            let end = (start + len).min(digits.len());
            let mut chunk: u64 = 0;
            let mut scale: u64 = 1;
            for &b in &digits[start..end] {
                chunk = chunk * 10 + u64::from(b.wrapping_sub(b'0'));
                scale *= 10;
            }
            // mag = mag * 10^len + chunk; `w * 10^9 + carry < 2^64`.
            let mut carry = chunk;
            for w in mag.iter_mut() {
                let v = u64::from(*w) * scale + carry;
                *w = v as u32;
                carry = v >> 32;
            }
            while carry > 0 {
                mag.push(carry as u32);
                carry >>= 32;
            }
            start = end;
            len = 9;
        }
        Self::normalize(mag, false)
    }

    /// `digits` (ASCII digits only) as a non-negative value: chunked up to
    /// `base_digits` digits, else `hi * 10^w + lo` with `w = 9 * 2^k` the
    /// largest such width below `digits.len()` (so `hi` has at most `w`
    /// digits and both halves are balanced for Karatsuba). `powers[k]` is
    /// `10^(9 * 2^k)`, built on first use.
    fn parse_dc(digits: &[u8], base_digits: usize, powers: &mut Vec<BigInt>) -> BigInt {
        if digits.len() <= base_digits {
            return Self::parse_chunked(digits);
        }
        let mut k = 0usize;
        while (9usize << (k + 1)) < digits.len() {
            k += 1;
        }
        let w = 9usize << k;
        let (hi, lo) = digits.split_at(digits.len() - w);
        let hi_v = Self::parse_dc(hi, base_digits, powers);
        let lo_v = Self::parse_dc(lo, base_digits, powers);
        if hi_v.is_zero() {
            return lo_v;
        }
        while powers.len() <= k {
            let next = match powers.last() {
                Some(p) => p.mul(p),
                None => BigInt::from_le_words(false, vec![1_000_000_000]),
            };
            powers.push(next);
        }
        hi_v.mul(&powers[k]).add(&lo_v)
    }

    /// Decimal rendering, `-` for a negative value. Magnitudes of at least
    /// [`TO_DECIMAL_DC_MIN_WORDS`] limbs take the subquadratic
    /// divide-and-conquer conversion ([`Self::to_decimal_dc`]), the rest the
    /// chunked loop.
    pub fn to_decimal(&self) -> String {
        if self.mag.len() >= TO_DECIMAL_DC_MIN_WORDS {
            if let Some(s) = self.to_decimal_dc(TO_DECIMAL_DC_BASE_WORDS, RECIPROCAL_BASE_BITS) {
                return s;
            }
        }
        self.to_decimal_chunked()
    }

    /// The chunked conversion: divide the whole magnitude by `10^9` once per
    /// nine digits, O(words^2) single-word steps.
    fn to_decimal_chunked(&self) -> String {
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
    // Subquadratic decimal conversion (round 13 wave 13, lane bigdec4;
    // proposal BD3-4 of `jit-r13-bigdec3-proposals-RETIRED-20260929.md`)
    // -----------------------------------------------------------------
    //
    // Split by `D_k = 10^(9 * 2^k)` (`D_0 = 10^9`, `D_k = D_{k-1}^2`): for
    // `x < D_k^2`, `x = q * D_k + r` with `q, r < D_k = D_{k-1}^2`, so both
    // halves recurse one level down; `r` is printed zero-padded to
    // `9 * 2^k` digits. With Knuth D as the division this is no faster than
    // the chunked loop (~n^2/2 word products), so each level divides by
    // Barrett reduction with the exact reciprocal `floor(2^(2L) / D_k)`
    // (`L = bits(D_k)`), computed once per level by Newton iteration from the
    // reciprocal of the divisor's top half: two Karatsuba products per node,
    // O(n^1.59) overall.
    //
    // CORRECTNESS DOES NOT REST ON THE ERROR BOUNDS. Every quotient estimate
    // is corrected against the exact remainder (`0 <= r < d`, checked), and
    // every reciprocal against the exact residual (`0 <= 2^(2L) - d * R < d`);
    // a correction that takes more than a few steps falls back to Knuth D.
    // The bounds only decide how fast the answer comes. `None` (a shift that
    // does not fit `u32`, or a broken width invariant) falls back to the
    // chunked loop.

    /// [`Self::to_decimal`]'s divide-and-conquer arm. `base_words`: pieces
    /// below this many limbs use the chunked loop; `recip_base_bits`:
    /// reciprocals of divisors up to this many bits are one Knuth division
    /// (the tests pass small values to drive every road on small inputs).
    fn to_decimal_dc(&self, base_words: usize, recip_base_bits: u64) -> Option<String> {
        if self.mag.is_empty() {
            return Some("0".to_string());
        }
        let x = self.abs_value();
        let bits = x.magnitude_bits();
        let mut levels: Vec<DecimalLevel> = Vec::new();
        let mut d = BigInt::from_le_words(false, vec![1_000_000_000]);
        loop {
            let l = d.magnitude_bits();
            // `D^2 >= 2^(2(l - 1))`, which exceeds `x` once `2(l - 1) >= bits`.
            let last = 2 * (l - 1) >= bits;
            let width = 9usize.checked_shl(u32::try_from(levels.len()).ok()?)?;
            levels.push(DecimalLevel {
                d: d.clone(),
                bits: l,
                width,
                recip: None,
            });
            if last {
                break;
            }
            d = d.mul(&d);
        }
        let mut out = String::with_capacity(usize::try_from(bits / 3 + 3).unwrap_or(0));
        if self.neg {
            out.push('-');
        }
        let top = levels.len() - 1;
        Self::dc_write(
            &x,
            top,
            None,
            &mut levels,
            base_words,
            recip_base_bits,
            &mut out,
        )?;
        Some(out)
    }

    /// Append `x` (`0 <= x < D_k^2`): exactly `width` digits, zero-padded,
    /// when `width` is given (it is then `2 * 9 * 2^k`), else without leading
    /// zeros.
    fn dc_write(
        x: &BigInt,
        k: usize,
        width: Option<usize>,
        levels: &mut [DecimalLevel],
        base_words: usize,
        recip_base_bits: u64,
        out: &mut String,
    ) -> Option<()> {
        if k == 0 || x.mag.len() < base_words {
            let s = x.to_decimal_chunked();
            if let Some(w) = width {
                if s.len() > w {
                    return None;
                }
                if x.is_zero() {
                    out.extend(std::iter::repeat_n('0', w));
                } else {
                    out.extend(std::iter::repeat_n('0', w - s.len()));
                    out.push_str(&s);
                }
            } else {
                out.push_str(&s);
            }
            return Some(());
        }
        let (q, r) = Self::dc_divmod(x, k, levels, recip_base_bits)?;
        let half = levels[k].width;
        match width {
            None if q.is_zero() => {
                Self::dc_write(&r, k - 1, None, levels, base_words, recip_base_bits, out)?;
            }
            None => {
                Self::dc_write(&q, k - 1, None, levels, base_words, recip_base_bits, out)?;
                Self::dc_write(
                    &r,
                    k - 1,
                    Some(half),
                    levels,
                    base_words,
                    recip_base_bits,
                    out,
                )?;
            }
            Some(_) => {
                Self::dc_write(
                    &q,
                    k - 1,
                    Some(half),
                    levels,
                    base_words,
                    recip_base_bits,
                    out,
                )?;
                Self::dc_write(
                    &r,
                    k - 1,
                    Some(half),
                    levels,
                    base_words,
                    recip_base_bits,
                    out,
                )?;
            }
        }
        Some(())
    }

    /// `x divmod D_k` for `0 <= x`, by Barrett reduction with the level's
    /// reciprocal, corrected to the exact answer.
    fn dc_divmod(
        x: &BigInt,
        k: usize,
        levels: &mut [DecimalLevel],
        recip_base_bits: u64,
    ) -> Option<(BigInt, BigInt)> {
        if levels[k].recip.is_none() {
            let r = Self::reciprocal(&levels[k].d, levels[k].bits, recip_base_bits)?;
            levels[k].recip = Some(r);
        }
        let level = &levels[k];
        let d = &level.d;
        let recip = level.recip.as_ref()?;
        let l = level.bits;
        // q^ = floor(floor(x / 2^(L-1)) * R / 2^(L+1)): q - 2 <= q^ <= q for
        // x < 2^(2L); corrected below either way.
        let q = x
            .shr(u32::try_from(l - 1).ok()?)
            .mul(recip)
            .shr(u32::try_from(l + 1).ok()?);
        let r = x.sub(&q.mul(d));
        Some(Self::correct_quotient(x, d, q, r))
    }

    /// `(q, r)` with `x = q * d + r` made exact (`0 <= r < d`) by stepping `q`;
    /// after four steps the estimate is abandoned for Knuth D.
    fn correct_quotient(x: &BigInt, d: &BigInt, mut q: BigInt, mut r: BigInt) -> (BigInt, BigInt) {
        let one = BigInt::from_le_words(false, vec![1]);
        let mut steps = 0;
        while r.is_neg() {
            steps += 1;
            if steps > 4 {
                return x.divmod(d);
            }
            r = r.add(d);
            q = q.sub(&one);
        }
        while r.cmp(d) != Ordering::Less {
            steps += 1;
            if steps > 4 {
                return x.divmod(d);
            }
            r = r.sub(d);
            q = q.add(&one);
        }
        (q, r)
    }

    /// `floor(2^(2l) / d)` exactly, for `2^(l-1) <= d < 2^l`. Up to
    /// `base_bits` one Knuth division; above, the reciprocal of `d`'s top `h`
    /// bits (`h = l/2 + 32`), shifted into place (relative error below
    /// `2^-(h-2)`), one Newton step `r += r * (2^(2l) - d*r) / 2^(2l)`
    /// (error squared, a few units), then exact by the residual.
    fn reciprocal(d: &BigInt, l: u64, base_bits: u64) -> Option<BigInt> {
        let two_l = u32::try_from(l.checked_mul(2)?).ok()?;
        let one = BigInt::from_le_words(false, vec![1]);
        let pow = one.shl(two_l);
        let h = l / 2 + 32;
        if l <= base_bits || h >= l {
            return Some(pow.div(d));
        }
        let drop = u32::try_from(l - h).ok()?;
        let rh = Self::reciprocal(&d.shr(drop), h, base_bits)?;
        let r0 = rh.shl(drop);
        let e0 = pow.sub(&d.mul(&r0));
        let r1 = r0.add(&r0.mul(&e0).shr(two_l));
        let e1 = pow.sub(&d.mul(&r1));
        let (r, _) = Self::correct_quotient(&pow, d, r1, e1);
        Some(r)
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

    /// `dst += src` in place, carry propagated. The caller guarantees the sum
    /// fits in `dst` (true for every partial product placed below); trailing
    /// zero limbs of `src` past `dst`'s end are ignored, and nothing indexes
    /// past either slice, so a violated guarantee truncates instead of
    /// panicking.
    fn add_into(dst: &mut [u32], src: &[u32]) {
        let mut n = src.len();
        while n > 0 && src[n - 1] == 0 {
            n -= 1;
        }
        let mut carry = 0u64;
        let mut i = 0;
        while i < n && i < dst.len() {
            let s = dst[i] as u64 + src[i] as u64 + carry;
            dst[i] = s as u32;
            carry = s >> 32;
            i += 1;
        }
        while carry != 0 && i < dst.len() {
            let s = dst[i] as u64 + carry;
            dst[i] = s as u32;
            carry = s >> 32;
            i += 1;
        }
    }

    /// `a -= b` in place, assuming `a >= b` as magnitudes.
    fn sub_in_place(a: &mut [u32], b: &[u32]) {
        let mut borrow = 0i64;
        let mut i = 0;
        while i < a.len() && (i < b.len() || borrow != 0) {
            let bv = if i < b.len() { b[i] as i64 } else { 0 };
            let mut d = a[i] as i64 - bv - borrow;
            if d < 0 {
                d += 1i64 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            a[i] = d as u32;
            i += 1;
        }
    }

    /// Karatsuba product of two magnitudes, `a.len() + b.len()` limbs, the
    /// same value [`Self::mul_mag`] computes. Pieces below
    /// [`KARATSUBA_THRESHOLD`] limbs go to the schoolbook kernel; an
    /// unbalanced pair (one operand at least twice the other) is cut into
    /// balanced slabs first. Every recursive call is on strictly shorter
    /// operands (the split point is `ceil(len/2)` and `len` is at least the
    /// threshold), so it terminates.
    fn mul_mag_karatsuba(a: &[u32], b: &[u32]) -> Vec<u32> {
        let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
        if b.len() < KARATSUBA_THRESHOLD {
            return Self::mul_mag(a, b);
        }
        let mut out = vec![0u32; a.len() + b.len()];
        if a.len() >= 2 * b.len() {
            let mut off = 0;
            while off < a.len() {
                let end = (off + b.len()).min(a.len());
                let part = Self::mul_mag_karatsuba(&a[off..end], b);
                Self::add_into(&mut out[off..], &part);
                off = end;
            }
            return out;
        }
        // b.len() <= a.len() < 2 * b.len(), so b.len() >= h and b1 may be
        // empty (only when a.len() is odd and b.len() == h).
        let h = a.len().div_ceil(2);
        let (a0, a1) = a.split_at(h);
        let (b0, b1) = b.split_at(h.min(b.len()));
        let z0 = Self::mul_mag_karatsuba(a0, b0);
        let z2 = Self::mul_mag_karatsuba(a1, b1);
        let sa = Self::add_mag(a0, a1);
        let sb = Self::add_mag(b0, b1);
        // z1 = (a0 + a1)(b0 + b1) - z0 - z2 = a0*b1 + a1*b0 >= 0.
        let mut z1 = Self::mul_mag_karatsuba(&sa, &sb);
        Self::sub_in_place(&mut z1, &z0);
        Self::sub_in_place(&mut z1, &z2);
        Self::add_into(&mut out, &z0);
        Self::add_into(&mut out[h..], &z1);
        Self::add_into(&mut out[2 * h..], &z2);
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

    pub fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => Self::cmp_mag(&self.mag, &o.mag),
            (true, true) => Self::cmp_mag(&o.mag, &self.mag),
        }
    }

    /// `BigInteger.abs()` — the magnitude with a non-negative sign.
    pub fn abs_value(&self) -> BigInt {
        BigInt {
            neg: false,
            mag: self.mag.clone(),
        }
    }

    pub fn neg_value(&self) -> BigInt {
        if self.is_zero() {
            BigInt::zero()
        } else {
            BigInt {
                neg: !self.neg,
                mag: self.mag.clone(),
            }
        }
    }

    pub fn add(&self, o: &BigInt) -> BigInt {
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

    pub fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.neg_value())
    }

    pub fn mul(&self, o: &BigInt) -> BigInt {
        if self.is_zero() || o.is_zero() {
            return BigInt::zero();
        }
        let wide = self.mag.len().min(o.mag.len()) >= KARATSUBA_THRESHOLD;
        let mag = if wide && fast_mul_enabled() {
            Self::mul_mag_karatsuba(&self.mag, &o.mag)
        } else {
            Self::mul_mag(&self.mag, &o.mag)
        };
        Self::normalize(mag, self.neg != o.neg)
    }

    /// Arithmetic (two's-complement, floor-toward-negative-infinity) left shift
    /// — i.e. `value * 2^n`. Matches `BigInteger.shiftLeft`.
    pub fn shl(&self, n: u32) -> BigInt {
        if self.is_zero() || n == 0 {
            return self.clone();
        }
        Self::normalize(Self::shl_mag(&self.mag, n as usize), self.neg)
    }

    /// Arithmetic right shift — `floor(value / 2^n)`. Matches
    /// `BigInteger.shiftRight`: for negative values this rounds toward negative
    /// infinity (so `-1 >> n == -1`, `-3 >> 1 == -2`), realized as
    /// `-ceil(|value| / 2^n)`.
    pub fn shr(&self, n: u32) -> BigInt {
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
    pub fn div(&self, o: &BigInt) -> BigInt {
        if o.is_zero() || self.is_zero() {
            return BigInt::zero();
        }
        let (q, _) = Self::divmod_mag(&self.mag, &o.mag);
        Self::normalize(q, self.neg != o.neg)
    }

    /// Remainder with the sign of the dividend — `BigInteger.remainder`.
    pub fn rem(&self, o: &BigInt) -> BigInt {
        if o.is_zero() || self.is_zero() {
            return BigInt::zero();
        }
        let (_, r) = Self::divmod_mag(&self.mag, &o.mag);
        Self::normalize(r, self.neg)
    }

    /// Truncated `(quotient, remainder)` together — `BigInteger.divideAndRemainder`.
    pub fn divmod(&self, o: &BigInt) -> (BigInt, BigInt) {
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
    pub fn modulo(&self, o: &BigInt) -> BigInt {
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
    pub fn modpow(&self, exp: &BigInt, modulus: &BigInt) -> BigInt {
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
    pub fn mod_inverse(&self, modulus: &BigInt) -> Option<BigInt> {
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
    pub fn is_probable_prime(&self) -> bool {
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
    pub fn magnitude_bits(&self) -> u64 {
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

    pub fn and(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x & y)
    }
    pub fn or(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x | y)
    }
    pub fn xor(&self, o: &BigInt) -> BigInt {
        self.bitop(o, |x, y| x ^ y)
    }
    /// `~self == -(self + 1)`.
    pub fn not(&self) -> BigInt {
        self.add(&Self::small(1)).neg_value()
    }

    /// `BigInteger.bitLength()` — bits in the minimal two's-complement
    /// representation, excluding the sign bit.
    pub fn bit_length(&self) -> u32 {
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
    pub fn bit_count(&self) -> u32 {
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
    pub fn test_bit(&self, n: u32) -> bool {
        (self.get_int((n / 32) as usize) >> (n % 32)) & 1 == 1
    }

    /// `BigInteger.getLowestSetBit()` — index of the rightmost set bit, or -1
    /// for zero. Same for both signs (two's-complement preserves the lowest set
    /// bit of the magnitude).
    pub fn lowest_set_bit(&self) -> i32 {
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
mod tests;

/// Round 13 wave 13 (lane bigdec4; proposal BD3-4): the divide-and-conquer
/// decimal conversion against the chunked loop, with its thresholds forced
/// down so the Barrett levels, the Newton reciprocals and the zero padding
/// run on small values, and at the shipping thresholds on wide ones.
#[cfg(test)]
mod r13w13_bigdec4_tests;

/// Round 14 wave 2 (lane bigdec; proposal BD4-1): the chunked and
/// divide-and-conquer parse against the digit-at-a-time loop, with the base
/// forced down so every recursion shape runs on short strings, and at the
/// shipping base on wide ones.
#[cfg(test)]
mod r14w2_bigdec_from_decimal_tests;
