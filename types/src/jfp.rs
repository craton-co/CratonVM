// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.Double` / `java.lang.Float` value semantics, transcribed once.
//!
//! # Why this module exists
//!
//! Rust's `f64` and Java's `double` agree on arithmetic and disagree on
//! *identity*. Java routes equality, hashing and ordering through
//! `doubleToLongBits`, which **collapses every NaN payload onto one canonical
//! pattern**; Rust's `f64::to_bits` is `doubleToRawLongBits`, which preserves
//! it. Three consequences, all silent-wrong-answer:
//!
//! * `Double.equals` is true for any two NaNs. With raw bits, `Math.sqrt(-1.0)`
//!   (`0xfff8…` on x86, on HotSpot too) is unequal to the `Double.NaN` constant
//!   (`0x7ff8…`), so an assertion comparing them fails while printing
//!   "expected NaN but was NaN".
//! * `Double.hashCode` is derived from the same canonical bits. Hash it from
//!   raw bits and two NaNs that `equals` land in different buckets, so a
//!   `HashMap` keyed on a NaN sentinel holds two entries where the JDK holds
//!   one — the `equals`/`hashCode` contract, broken.
//! * `Double.compare` canonicalizes first, so all NaNs are equal to each other
//!   and greater than everything else. `f64::total_cmp` is a *different* total
//!   order — IEEE 754 totalOrder, which sorts a negatively-signed NaN BELOW
//!   `-inf` and orders NaNs among themselves by payload. The two agree on
//!   `-0.0 < +0.0` and on NaN-versus-number, which is exactly why substituting
//!   one for the other looks right and passes every test that does not put a
//!   negative or payload-carrying NaN in a sorted collection.
//!
//! # Why in `cratonvm-types` rather than beside a caller
//!
//! Before this module the VM carried **five** transcriptions of these rules
//! across three crates, and they did not agree with each other: two correct
//! (`lang_math`'s wrapper natives, `native-collections`' `DoubleStream` helper),
//! one raw-bits (`native-collections`' `double_compare`), and two `total_cmp`
//! (`natural_compare`, `Arrays.parallelSort([D)`). A defect fixed in one was
//! still live in the other four, which is what happened: the wrapper natives
//! were repaired for the commons-math `expected NaN but was NaN` cluster and
//! `Comparator.naturalOrder()` kept its own broken copy.
//!
//! Every site that needs these semantics calls this module. The functions are
//! `#[inline]` and branch-free apart from one `is_nan` test, so there is no
//! reason for a caller to hand-roll a faster one.

/// `Double.doubleToLongBits(double)` — the canonicalizing conversion.
///
/// Not `f64::to_bits`, which is `doubleToRawLongBits`.
#[inline]
#[must_use]
pub fn double_to_long_bits(v: f64) -> u64 {
    if v.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        v.to_bits()
    }
}

/// `Float.floatToIntBits(float)` — the canonicalizing conversion.
#[inline]
#[must_use]
pub fn float_to_int_bits(v: f32) -> u32 {
    if v.is_nan() {
        0x7fc0_0000
    } else {
        v.to_bits()
    }
}

/// `Double.compare(double, double)`.
///
/// A total order with `-0.0 < +0.0` and every NaN equal to every other NaN and
/// greater than every number. See the module note on why `f64::total_cmp` is
/// not this function.
#[inline]
#[must_use]
pub fn double_compare(a: f64, b: f64) -> i32 {
    if a < b {
        return -1;
    }
    if a > b {
        return 1;
    }
    // Equal under `<`/`>` — which covers the two zeros and every NaN pairing.
    // The canonical bits are compared as SIGNED longs, exactly as
    // `Double.compare` does, so `-0.0` (`0x8000…`, negative as a long) orders
    // below `+0.0` and the canonical NaN (`0x7ff8…`, the largest positive
    // pattern here) orders above every finite value.
    let ab = double_to_long_bits(a) as i64;
    let bb = double_to_long_bits(b) as i64;
    match ab.cmp(&bb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `Float.compare(float, float)` — see the double version.
#[inline]
#[must_use]
pub fn float_compare(a: f32, b: f32) -> i32 {
    if a < b {
        return -1;
    }
    if a > b {
        return 1;
    }
    let ab = float_to_int_bits(a) as i32;
    let bb = float_to_int_bits(b) as i32;
    match ab.cmp(&bb) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `Double.compare` as a [`std::cmp::Ordering`], for `sort_by` callers.
#[inline]
#[must_use]
pub fn double_ordering(a: f64, b: f64) -> std::cmp::Ordering {
    double_compare(a, b).cmp(&0)
}

/// `Float.compare` as a [`std::cmp::Ordering`].
#[inline]
#[must_use]
pub fn float_ordering(a: f32, b: f32) -> std::cmp::Ordering {
    float_compare(a, b).cmp(&0)
}

/// `Double.hashCode(double)` — `(bits ^ (bits >>> 32))` over the CANONICAL bits.
#[inline]
#[must_use]
pub fn double_hash_code(v: f64) -> i32 {
    let bits = double_to_long_bits(v);
    (bits ^ (bits >> 32)) as i32
}

/// `Float.hashCode(float)` — the canonical bits, reinterpreted as `int`.
#[inline]
#[must_use]
pub fn float_hash_code(v: f32) -> i32 {
    float_to_int_bits(v) as i32
}

/// `Double.equals` — bit equality after canonicalization.
///
/// Note the two ways this differs from `a == b`: NaN equals itself, and `+0.0`
/// does NOT equal `-0.0`.
#[inline]
#[must_use]
pub fn double_equals(a: f64, b: f64) -> bool {
    double_to_long_bits(a) == double_to_long_bits(b)
}

/// `Float.equals` — see the double version.
#[inline]
#[must_use]
pub fn float_equals(a: f32, b: f32) -> bool {
    float_to_int_bits(a) == float_to_int_bits(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three NaNs a real program actually produces, and the rules that
    /// separate this module from the two Rust primitives it is easy to reach
    /// for instead.
    #[test]
    fn every_nan_is_one_value_for_equality_hashing_and_ordering() {
        let quiet = f64::from_bits(0x7ff8_0000_0000_0000);
        let signed = f64::from_bits(0xfff8_0000_0000_0000); // what `sqrt(-1.0)` gives on x86
        let payload = f64::from_bits(0x7ff8_0000_0000_0001);
        let all_ones = f64::from_bits(0x7fff_ffff_ffff_ffff);

        for &a in &[quiet, signed, payload, all_ones] {
            for &b in &[quiet, signed, payload, all_ones] {
                assert!(double_equals(a, b), "{a:?} vs {b:?}");
                assert_eq!(double_compare(a, b), 0);
                assert_eq!(double_hash_code(a), double_hash_code(b));
            }
            // Greater than every number, including +inf — this is the clause
            // `f64::total_cmp` gets wrong for `signed`, where it answers "less
            // than -inf".
            assert_eq!(double_compare(a, f64::INFINITY), 1);
            assert_eq!(double_compare(f64::NEG_INFINITY, a), -1);
        }
        assert_eq!(
            signed.total_cmp(&f64::NEG_INFINITY),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn signed_zero_is_two_values() {
        assert!(!double_equals(0.0, -0.0));
        assert_eq!(double_compare(-0.0, 0.0), -1);
        assert_eq!(double_compare(0.0, -0.0), 1);
        assert_eq!(double_compare(0.0, 0.0), 0);
        assert_ne!(double_hash_code(0.0), double_hash_code(-0.0));
        // ...but `==` says they are the same, which is why a map keyed on them
        // with plain `==` silently loses one.
        assert!(0.0 == -0.0);
    }

    #[test]
    fn ordinary_values_are_unaffected() {
        assert_eq!(double_compare(1.0, 2.0), -1);
        assert_eq!(double_compare(2.0, 1.0), 1);
        assert_eq!(double_compare(-5.5, -5.5), 0);
        assert_eq!(double_to_long_bits(1.0), 0x3ff0_0000_0000_0000);
        assert_eq!(double_hash_code(1.0), 0x3ff0_0000);
    }

    #[test]
    fn float_mirrors_double() {
        let quiet = f32::from_bits(0x7fc0_0000);
        let signed = f32::from_bits(0xffc0_0000);
        let payload = f32::from_bits(0x7fc0_0001);
        for &a in &[quiet, signed, payload] {
            for &b in &[quiet, signed, payload] {
                assert!(float_equals(a, b));
                assert_eq!(float_compare(a, b), 0);
                assert_eq!(float_hash_code(a), float_hash_code(b));
            }
            assert_eq!(float_compare(a, f32::INFINITY), 1);
        }
        assert!(!float_equals(0.0, -0.0));
        assert_eq!(float_compare(-0.0, 0.0), -1);
    }

    /// The three ways this rule gets written wrong, side by side.
    ///
    /// `f64::total_cmp` and `partial_cmp(..).unwrap_or(Equal)` are the two
    /// substitutions that have actually shipped in this tree, and each is wrong
    /// in its own direction. Naming both here means a reader who reaches for
    /// one finds the counter-example before the census does.
    #[test]
    fn neither_total_cmp_nor_partial_cmp_is_double_compare() {
        let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);

        // total_cmp: orders a negatively-signed NaN BELOW -infinity.
        assert_eq!(double_compare(neg_nan, f64::NEG_INFINITY), 1);
        assert_eq!(
            neg_nan.total_cmp(&f64::NEG_INFINITY),
            std::cmp::Ordering::Less
        );

        // partial_cmp: `None` for any NaN, so the usual `unwrap_or(Equal)`
        // makes NaN equal to everything — not merely the wrong order, but a
        // NON-TRANSITIVE comparator: NaN == 1.0 and NaN == 2.0 while 1.0 != 2.0.
        assert_eq!(neg_nan.partial_cmp(&1.0), None);
        assert_eq!(double_compare(neg_nan, 1.0), 1);
        assert_eq!(double_compare(1.0, neg_nan), -1);

        // ...and it is wrong with no NaN in sight, which is the reachable half:
        // `partial_cmp` calls the two zeros equal because `-0.0 == 0.0`.
        assert_eq!((-0.0f64).partial_cmp(&0.0), Some(std::cmp::Ordering::Equal));
        assert_eq!(double_compare(-0.0, 0.0), -1);
    }

    /// A sort is the shape that actually reached users, so assert on one.
    #[test]
    fn sorting_puts_every_nan_at_the_top_whatever_its_payload() {
        let mut v = vec![
            f64::from_bits(0xfff8_0000_0000_0000),
            f64::NEG_INFINITY,
            -0.0,
            0.0,
            1.0,
            f64::INFINITY,
            f64::from_bits(0x7ff8_0000_0000_0001),
        ];
        v.sort_by(|a, b| double_ordering(*a, *b));
        let bits: Vec<u64> = v.iter().map(|x| x.to_bits()).collect();
        assert_eq!(
            bits,
            vec![
                0xfff0_0000_0000_0000, // -inf
                0x8000_0000_0000_0000, // -0.0
                0x0000_0000_0000_0000, // +0.0
                0x3ff0_0000_0000_0000, // 1.0
                0x7ff0_0000_0000_0000, // +inf
                // The two NaNs keep their payloads — only the ORDER is
                // canonical — and land above +inf. Under `total_cmp` the
                // negative one would have sorted first, below -inf.
                0xfff8_0000_0000_0000,
                0x7ff8_0000_0000_0001,
            ]
        );
    }
}
