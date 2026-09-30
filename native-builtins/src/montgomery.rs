// SPDX-License-Identifier: Apache-2.0
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
pub(crate) struct Montgomery {
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
    pub(crate) fn new(m: &[u32]) -> Option<Montgomery> {
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
    pub(crate) fn limbs(&self) -> usize {
        self.m.len()
    }

    /// The modulus limbs.
    pub(crate) fn modulus(&self) -> &[u32] {
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
    pub(crate) fn mul(&self, a: &[u32], b: &[u32]) -> Vec<u32> {
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
    pub(crate) fn out_of_mont(&self, a: &[u32]) -> Vec<u32> {
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
pub(crate) fn bit_len(mag: &[u32]) -> usize {
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
pub(crate) fn modpow_odd(
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
mod tests {
    use super::*;
    use crate::bigint::BigInt;

    /// Schoolbook `a * b mod m` on limb slices, via `BigInt` — the oracle the
    /// Montgomery product is checked against.
    fn ref_mulmod(a: &[u32], b: &[u32], m: &[u32]) -> Vec<u32> {
        let ba = BigInt::from_le_words(false, a.to_vec());
        let bb = BigInt::from_le_words(false, b.to_vec());
        let bm = BigInt::from_le_words(false, m.to_vec());
        ba.mul(&bb).modulo(&bm).mag_le().to_vec()
    }

    fn pad(v: &[u32], n: usize) -> Vec<u32> {
        let mut o = v.to_vec();
        o.resize(n, 0);
        o
    }

    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn rand_odd(state: &mut u64, limbs: usize) -> Vec<u32> {
        let mut v: Vec<u32> = (0..limbs).map(|_| xorshift(state) as u32).collect();
        v[0] |= 1; // odd
        let top = limbs - 1;
        v[top] |= 1 << 31; // full width, so `n` is what we asked for
        v
    }

    /// `R mod m` and `R^2 mod m` for a context, via the division path.
    fn radix_constants(m: &[u32], n: usize) -> (Vec<u32>, Vec<u32>) {
        let bm = BigInt::from_le_words(false, m.to_vec());
        let one = BigInt::from_le_words(false, vec![1]);
        let r1 = pad(one.shl(32 * n as u32).modulo(&bm).mag_le(), n);
        let r2 = pad(one.shl(64 * n as u32).modulo(&bm).mag_le(), n);
        (r1, r2)
    }

    #[test]
    fn neg_inv_is_the_inverse_of_every_odd_word() {
        let mut state = 0x1234_5678_9abc_def1u64;
        for _ in 0..2000 {
            let m0 = (xorshift(&mut state) as u32) | 1;
            let ni = neg_inv_2_32(m0);
            // -m0^-1 * m0 == -1 == 0xFFFF_FFFF (mod 2^32)
            assert_eq!(ni.wrapping_mul(m0), u32::MAX, "neg_inv for {m0:#x}");
        }
        assert_eq!(neg_inv_2_32(1).wrapping_mul(1), u32::MAX);
        assert_eq!(neg_inv_2_32(u32::MAX).wrapping_mul(u32::MAX), u32::MAX);
    }

    #[test]
    fn new_rejects_moduli_with_no_montgomery_form() {
        assert!(Montgomery::new(&[]).is_none(), "zero");
        assert!(Montgomery::new(&[0, 0]).is_none(), "zero, padded");
        assert!(Montgomery::new(&[1]).is_none(), "one");
        assert!(Montgomery::new(&[2]).is_none(), "even");
        assert!(Montgomery::new(&[4, 7]).is_none(), "even, multi-limb");
        assert!(Montgomery::new(&[0, 1]).is_none(), "2^32, even");
        assert!(Montgomery::new(&[3]).is_some());
        // Trailing zero limbs are trimmed, not counted.
        let m = Montgomery::new(&[7, 0, 0]).unwrap();
        assert_eq!(m.limbs(), 1);
    }

    #[test]
    fn mul_matches_schoolbook_mulmod_over_random_operands() {
        let mut state = 0xdead_beef_0bad_f00du64;
        for limbs in 1..=9 {
            for _ in 0..40 {
                let m = rand_odd(&mut state, limbs);
                let mont = Montgomery::new(&m).unwrap();
                let n = mont.limbs();
                let (r1, r2) = radix_constants(&m, n);
                // 1 in Montgomery form really is R mod m.
                assert_eq!(mont.out_of_mont(&r1), pad(&[1], n), "r1 is 1*R");

                for _ in 0..8 {
                    let a: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                    let b: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                    let ba = pad(&ref_mulmod(&a, &[1], &m), n);
                    let bb = pad(&ref_mulmod(&b, &[1], &m), n);
                    // mont.mul(a*R, b*R) == (a*b)*R
                    let am = mont.mul(&ba, &r2);
                    let bmm = mont.mul(&bb, &r2);
                    let prod = mont.mul(&am, &bmm);
                    assert_eq!(
                        mont.out_of_mont(&prod),
                        pad(&ref_mulmod(&ba, &bb, &m), n),
                        "montgomery mul, {limbs} limbs"
                    );
                    // Round-tripping through Montgomery form is the identity.
                    assert_eq!(mont.out_of_mont(&am), ba, "to/from mont round trip");
                    // The result is always fully reduced.
                    assert_eq!(
                        cmp_mag(&prod, mont.modulus()),
                        std::cmp::Ordering::Less,
                        "mul result not reduced"
                    );
                }
                // Boundary operands: 0, 1, m-1.
                let zero = vec![0u32; n];
                let mut mm1 = m.clone();
                mm1[0] -= 1; // m odd, so m-1 needs no borrow
                let cases = [zero, pad(&[1], n), mm1];
                for a in &cases {
                    for b in &cases {
                        let am = mont.mul(a, &r2);
                        let bmn = mont.mul(b, &r2);
                        let got = mont.out_of_mont(&mont.mul(&am, &bmn));
                        assert_eq!(got, pad(&ref_mulmod(a, b, &m), n), "boundary mul");
                    }
                }
            }
        }
    }

    /// The window machinery is where an off-by-one hides. Drive `modpow_odd`
    /// across every exponent bit length that changes the window width, against
    /// a reference square-and-multiply that shares none of its code.
    #[test]
    fn modpow_odd_matches_square_and_multiply_across_window_widths() {
        fn ref_modpow(base: &[u32], exp: &[u32], m: &[u32]) -> Vec<u32> {
            let bm = BigInt::from_le_words(false, m.to_vec());
            let mut result = BigInt::from_le_words(false, vec![1]).modulo(&bm);
            let mut b = BigInt::from_le_words(false, base.to_vec()).modulo(&bm);
            for i in 0..bit_len(exp) {
                if (exp[i / 32] >> (i % 32)) & 1 == 1 {
                    result = result.mul(&b).modulo(&bm);
                }
                b = b.mul(&b).modulo(&bm);
            }
            result.mag_le().to_vec()
        }

        let mut state = 0x0f0f_1e1e_2d2d_3c3cu64;
        for limbs in [1usize, 2, 3, 5, 8] {
            let m = rand_odd(&mut state, limbs);
            let mont = Montgomery::new(&m).unwrap();
            let n = mont.limbs();
            let (r1, r2) = radix_constants(&m, n);
            // Every `window_width` boundary, plus one either side of each.
            let widths = [
                1usize, 2, 3, 4, 5, 6, 23, 24, 25, 26, 95, 96, 97, 98, 319, 320, 321, 322, 959,
                960, 961, 962, 2048, 2687, 2688, 2689, 2690,
            ];
            for &ebits in &widths {
                for trial in 0..3 {
                    // Exponent with exactly `ebits` bits.
                    let ewords = ebits.div_ceil(32);
                    let mut e: Vec<u32> =
                        (0..ewords).map(|_| xorshift(&mut state) as u32).collect();
                    let topbit = (ebits - 1) % 32;
                    e[ewords - 1] &= (1u32 << topbit) - 1 | (1u32 << topbit);
                    e[ewords - 1] |= 1u32 << topbit;
                    if trial == 1 {
                        // All-ones exponent: every window is full.
                        for w in e.iter_mut() {
                            *w = u32::MAX;
                        }
                        e[ewords - 1] = if topbit == 31 {
                            u32::MAX
                        } else {
                            (1u32 << (topbit + 1)) - 1
                        };
                    } else if trial == 2 {
                        // Single set bit: every window but one is zero.
                        for w in e.iter_mut() {
                            *w = 0;
                        }
                        e[ewords - 1] = 1u32 << topbit;
                    }
                    assert_eq!(bit_len(&e), ebits, "exponent construction");

                    let base: Vec<u32> = (0..n).map(|_| xorshift(&mut state) as u32).collect();
                    let base_mod = ref_mulmod(&base, &[1], &m);
                    let got = modpow_odd(&mont, &base_mod, &e, &r1, &r2);
                    let want = ref_modpow(&base_mod, &e, &m);
                    assert_eq!(got, want, "modpow {limbs} limbs, {ebits}-bit exponent");
                }
            }
        }
    }

    #[test]
    fn modpow_odd_handles_degenerate_bases_and_exponents() {
        let mut state = 0x7777_8888_9999_aaaau64;
        for limbs in 1..=4 {
            let m = rand_odd(&mut state, limbs);
            let mont = Montgomery::new(&m).unwrap();
            let n = mont.limbs();
            let (r1, r2) = radix_constants(&m, n);
            let mut mm1 = m.clone();
            mm1[0] -= 1;

            // x^0 == 1 for every base, including 0.
            for base in [vec![], vec![1], vec![2], mm1.clone()] {
                assert_eq!(modpow_odd(&mont, &base, &[], &r1, &r2), vec![1], "x^0 == 1");
                assert_eq!(
                    modpow_odd(&mont, &base, &[0, 0], &r1, &r2),
                    vec![1],
                    "x^0 == 1, padded exponent"
                );
            }
            // 0^e == 0 for e > 0.
            for e in [vec![1u32], vec![2], vec![0xFFFF_FFFF, 0xFFFF_FFFF]] {
                assert_eq!(modpow_odd(&mont, &[], &e, &r1, &r2), Vec::<u32>::new());
            }
            // 1^e == 1.
            assert_eq!(modpow_odd(&mont, &[1], &[0xDEAD_BEEF], &r1, &r2), vec![1]);
            // x^1 == x.
            let base = ref_mulmod(&[0x1234_5678, 0x9abc_def0], &[1], &m);
            assert_eq!(modpow_odd(&mont, &base, &[1], &r1, &r2), base);
        }
    }

    #[test]
    fn bit_len_counts_the_highest_set_bit() {
        assert_eq!(bit_len(&[]), 0);
        assert_eq!(bit_len(&[0]), 0);
        assert_eq!(bit_len(&[0, 0, 0]), 0);
        assert_eq!(bit_len(&[1]), 1);
        assert_eq!(bit_len(&[2]), 2);
        assert_eq!(bit_len(&[0xFFFF_FFFF]), 32);
        assert_eq!(bit_len(&[0, 1]), 33);
        assert_eq!(bit_len(&[0xFFFF_FFFF, 0xFFFF_FFFF]), 64);
        assert_eq!(bit_len(&[5, 1, 0, 0]), 33);
    }

    #[test]
    fn window_at_reads_little_endian_bit_runs() {
        // 0b1011_0110 = 0xB6
        let e = [0xB6u32];
        assert_eq!(window_at(&e, 0, 4), 0x6);
        assert_eq!(window_at(&e, 4, 4), 0xB);
        assert_eq!(window_at(&e, 6, 4), 0x2); // bits 6,7 = 1,0 -> 0b0010
        assert_eq!(window_at(&e, 32, 4), 0); // past the end
                                             // A window straddling the word boundary.
        let e2 = [0x8000_0000u32, 0x0000_0001];
        assert_eq!(window_at(&e2, 31, 2), 0b11);
        assert_eq!(window_at(&e2, 30, 4), 0b0110);
    }

    #[test]
    fn window_width_is_monotone_and_bounded() {
        let mut prev = 0;
        for bits in [
            1usize, 4, 5, 24, 25, 96, 97, 320, 321, 960, 961, 2688, 2689, 8192,
        ] {
            let w = window_width(bits);
            assert!(w >= prev, "width regressed at {bits}");
            assert!((1..=7).contains(&w), "width {w} out of range at {bits}");
            prev = w;
        }
    }
}
