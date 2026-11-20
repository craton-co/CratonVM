// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Montgomery modular arithmetic over base-2^32 little-endian limbs.
//!
//! Retires `perf/biginteger-modpow-has-no-montgomery-reduction-20260817`: the
//! two limb bignums in this crate (`crate::bigint::BigInt`, backing
//! `java.math.BigInteger.modPow`, and `crate::crypto_impl::BigUint`, backing
//! the native RSA sign/verify path) both ran textbook square-and-multiply with
//! a **full Knuth-D division per step**. Montgomery reduction replaces that
//! division with a multiply and a word shift.
//!
//! This module is deliberately *type-free*: it works on `&[u32]` limb slices so
//! there is exactly one implementation, one set of invariants, and one
//! differential test surface for both callers. Nothing here allocates a
//! `BigInt`/`BigUint` or knows about signs — the callers own sign handling and
//! own the "modulus must be odd" precondition.
//!
//! ## Preconditions (checked by the constructor, not assumed)
//!
//! * the modulus is **odd** and > 1 — Montgomery needs `gcd(R, m) == 1` with
//!   `R = 2^(32n)`, which for a power-of-two `R` means exactly "m is odd". An
//!   even modulus has no Montgomery form at all; `Montgomery::new` returns
//!   `None` and the caller falls back to division-based square-and-multiply.
//! * operands handed to [`Montgomery::mul`] are already in Montgomery form and
//!   strictly less than the modulus.

/// Precomputed Montgomery context for one odd modulus.
///
/// `R = 2^(32 * n)` where `n` is the modulus limb count. A residue `x` is held
/// in Montgomery form as `x * R mod m`, in exactly `n` limbs, always reduced
/// into `[0, m)`.
pub struct Montgomery {
    /// The modulus, normalized (no trailing zero limbs), `n` limbs, odd, > 1.
    m: Vec<u32>,
    /// `-m^-1 mod 2^32`. The multiplier that zeroes one limb per reduction step.
    n0inv: u32,
}

impl Montgomery {
    /// Build a context for an odd modulus > 1. `m` is the little-endian
    /// magnitude; trailing zero limbs are trimmed. Returns `None` for a zero,
    /// one, or **even** modulus — the cases with no Montgomery form, which the
    /// caller must handle with the division-based path.
    pub fn new(m: &[u32]) -> Option<Montgomery> {
        let mut n = m.len();
        while n > 0 && m[n - 1] == 0 {
            n -= 1;
        }
        if n == 0 {
            return None; // zero modulus
        }
        if m[0] & 1 == 0 {
            return None; // even modulus: gcd(2^k, m) != 1, no Montgomery form
        }
        if n == 1 && m[0] == 1 {
            return None; // modulus 1: every residue is 0, degenerate
        }
        Some(Montgomery {
            m: m[..n].to_vec(),
            n0inv: neg_inv_2_32(m[0]),
        })
    }

    /// Modulus limb count — the width of every Montgomery-form value.
    pub fn limbs(&self) -> usize {
        self.m.len()
    }

    /// The modulus limbs.
    pub fn modulus(&self) -> &[u32] {
        &self.m
    }

    /// Montgomery product: `a * b * R^-1 mod m`, given `a, b < m` in `n` limbs.
    ///
    /// Koc's CIOS (Coarsely Integrated Operand Scanning) — the interleaved form
    /// that never materializes the `2n`-limb product, so the working set is
    /// `n + 1` limbs.
    ///
    /// Loop invariant: `t < 2m` at the top of every iteration. Each step adds
    /// `a * b[i] + m * mi <= (m-1)(2^32-1) + m(2^32-1)` and then shifts down by
    /// one limb, so `t' <= ((2m-1) + (2m-1)(2^32-1)) / 2^32 = 2m-1`. That bound
    /// is why one conditional subtraction at the end suffices, and why `top`
    /// below is provably 0 or 1.
    pub fn mul(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
        let n = self.m.len();
        debug_assert_eq!(a.len(), n, "montgomery operand width");
        debug_assert_eq!(b.len(), n, "montgomery operand width");
        let mut t = vec![0u32; n + 1];
        for i in 0..n {
            let bi = b[i] as u64;
            // t += a * b[i]
            let mut carry: u64 = 0;
            for j in 0..n {
                // t[j] + a[j]*b[i] + carry <= (2^32-1) + (2^32-1)^2 + (2^32-1)
                //                           = 2^64 - 1, so u64 never overflows.
                let v = (t[j] as u64) + (a[j] as u64) * bi + carry;
                t[j] = v as u32;
                carry = v >> 32;
            }
            let v = (t[n] as u64) + carry;
            t[n] = v as u32;
            let mut top = v >> 32;

            // t += m * mi, with mi chosen so the low limb of t becomes zero.
            let mi = t[0].wrapping_mul(self.n0inv) as u64;
            let mut carry2: u64 = 0;
            for j in 0..n {
                let v = (t[j] as u64) + mi * (self.m[j] as u64) + carry2;
                t[j] = v as u32;
                carry2 = v >> 32;
            }
            let v = (t[n] as u64) + carry2;
            t[n] = v as u32;
            top += v >> 32;
            debug_assert_eq!(t[0], 0, "CIOS reduction did not clear the low limb");
            debug_assert!(top <= 1, "CIOS overflow past limb n+1");

            // t >>= 32
            t.copy_within(1..=n, 0);
            t[n] = top as u32;
        }
        // t < 2m: at most one subtraction brings it into [0, m).
        if t[n] != 0 || cmp_mag(&t[..n], &self.m) != std::cmp::Ordering::Less {
            sub_in_place(&mut t, &self.m);
        }
        t.truncate(n);
        t
    }

    /// `x * R^-1 mod m` — the conversion *out* of Montgomery form.
    ///
    /// `out_of_mont` rather than `from_mont`: `clippy::wrong_self_convention`
    /// reserves a `from_*` name for an associated function taking no `self`,
    /// and this one is a method on the modulus context.
    pub fn out_of_mont(&self, a: &[u32]) -> Vec<u32> {
        let n = self.m.len();
        let mut one = vec![0u32; n];
        one[0] = 1;
        self.mul(a, &one)
    }
}

/// `-m0^-1 mod 2^32` for odd `m0`, by Newton-Hensel lifting.
///
/// `inv = 1` is already the inverse mod 2 (any odd `m0` has `m0 * 1 == 1 mod
/// 2`), and `inv *= 2 - m0*inv` doubles the correct bit count each round:
/// 2, 4, 8, 16, 32 — hence exactly five rounds for a 32-bit word.
fn neg_inv_2_32(m0: u32) -> u32 {
    debug_assert!(m0 & 1 == 1, "neg_inv_2_32 needs an odd word");
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(inv)));
    }
    debug_assert_eq!(m0.wrapping_mul(inv), 1);
    inv.wrapping_neg()
}

/// Unsigned limb-slice comparison. Both sides must be the same length.
fn cmp_mag(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    debug_assert_eq!(a.len(), b.len());
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    std::cmp::Ordering::Equal
}

/// `t -= m`, where `t` is one limb wider than `m` and `t >= m`.
fn sub_in_place(t: &mut [u32], m: &[u32]) {
    let mut borrow: i64 = 0;
    for i in 0..t.len() {
        let mv = *m.get(i).unwrap_or(&0) as i64;
        let mut d = (t[i] as i64) - mv - borrow;
        if d < 0 {
            d += 1i64 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        t[i] = d as u32;
    }
    debug_assert_eq!(borrow, 0, "sub_in_place underflow: t < m");
}

/// Bit length of a little-endian limb magnitude (index of the highest set bit
/// plus one; 0 for zero). The fix for the `mag.len() * 32` bound that squared
/// up to 31 leading zero bits for nothing.
pub fn bit_len(mag: &[u32]) -> usize {
    let mut n = mag.len();
    while n > 0 && mag[n - 1] == 0 {
        n -= 1;
    }
    if n == 0 {
        0
    } else {
        (n - 1) * 32 + (32 - mag[n - 1].leading_zeros() as usize)
    }
}

/// The `w` exponent bits starting at bit `lo` (little-endian bit order), zero
/// past the end. `w <= 32`.
fn window_at(exp: &[u32], lo: usize, w: usize) -> usize {
    debug_assert!(w <= 32);
    let mut out = 0usize;
    for k in 0..w {
        let bit = lo + k;
        let word = bit / 32;
        if word < exp.len() && (exp[word] >> (bit % 32)) & 1 == 1 {
            out |= 1 << k;
        }
    }
    out
}

/// Window width for an exponent of `ebits` bits.
///
/// A full `2^w`-entry table costs `2^w - 2` Montgomery multiplies to build and
/// then needs `ebits / w` of them instead of `ebits / 2` on average, so the
/// total multiply count is `(2^w - 2) + ebits/w` (the squaring count is
/// `ebits` either way). Setting that equal for `w` and `w+1` puts the crossover
/// at `ebits = 2^w * w * (w+1)`: 4, 24, 96, 320, 960, 2688 — the table below.
///
/// Note these are *not* the JDK's `BigInteger.oddModPow` thresholds, which are
/// tuned for a sliding window over a half-size odd-powers-only table. Copying
/// those here would over-widen the window and spend more on the table than the
/// window saves — at a 2048-bit exponent the JDK's `w = 7` costs ~4% more
/// multiplies than the `w = 6` this picks.
fn window_width(ebits: usize) -> usize {
    match ebits {
        0..=4 => 1,
        5..=24 => 2,
        25..=96 => 3,
        97..=320 => 4,
        321..=960 => 5,
        961..=2688 => 6,
        // Capped at 7: the next crossover is at ~7700 bits, and the table would
        // be 256 entries of modulus width.
        _ => 7,
    }
}

/// `base^exp mod m` for an **odd** modulus, as limb magnitudes.
///
/// `base_mod` must already be reduced into `[0, m)`; `exp` is a non-negative
/// magnitude; `r1` is `R mod m` (the Montgomery form of 1) and `r2` is
/// `R^2 mod m`, both `n` limbs — the caller computes them because only the
/// caller has a division. Returns the normalized (trailing zeros trimmed)
/// result magnitude.
///
/// Fixed-window left-to-right: `ebits` rounded up to a multiple of `w`
/// squarings and `ebits / w` multiplies, against a `2^w`-entry table.
pub fn modpow_odd(
    mont: &Montgomery,
    base_mod: &[u32],
    exp: &[u32],
    r1: &[u32],
    r2: &[u32],
) -> Vec<u32> {
    let n = mont.limbs();
    debug_assert_eq!(r1.len(), n);
    debug_assert_eq!(r2.len(), n);

    let ebits = bit_len(exp);
    if ebits == 0 {
        // x^0 == 1 mod m, and m > 1 is guaranteed by Montgomery::new.
        return vec![1];
    }

    let mut base_padded = base_mod.to_vec();
    base_padded.resize(n, 0);
    // x -> x*R mod m
    let base_mont = mont.mul(&base_padded, r2);

    let w = window_width(ebits);
    let table_len = 1usize << w;
    let mut table: Vec<Vec<u32>> = Vec::with_capacity(table_len);
    table.push(r1.to_vec()); // table[0] = 1 in Montgomery form
    if table_len > 1 {
        table.push(base_mont.clone());
    }
    for k in 2..table_len {
        table.push(mont.mul(&table[k - 1], &base_mont));
    }

    let mut acc = r1.to_vec();
    let nwindows = ebits.div_ceil(w);
    let mut started = false;
    for widx in (0..nwindows).rev() {
        let idx = window_at(exp, widx * w, w);
        if !started {
            // acc is still 1: squaring it is a no-op, and the first non-zero
            // window can be read straight out of the table.
            if idx == 0 {
                continue;
            }
            acc = table[idx].clone();
            started = true;
            continue;
        }
        for _ in 0..w {
            acc = mont.mul(&acc, &acc);
        }
        if idx != 0 {
            acc = mont.mul(&acc, &table[idx]);
        }
    }

    let mut out = mont.out_of_mont(&acc);
    while out.last() == Some(&0) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests;
