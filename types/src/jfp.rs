// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// `Math.min(double, double)` / the fold `DoubleStream.min()` needs.
///
/// Rust's `f64::min` is IEEE-754-2019 `minNum`, which IGNORES a NaN operand
/// and returns the other one; Java's `Math.min` PROPAGATES it, and the two
/// also disagree on signed zero (`min(+0.0, -0.0)` is `-0.0` in Java, and
/// unspecified — implementation-defined either way — for `f64::min`). See
/// `native-builtins::lang_math`'s `java_min_double`, the first place this
/// exact rule was transcribed; this is the copy `native-collections`'
/// `DoubleStream.min()` can reach without a `native-builtins` dependency,
/// which `native-builtins` itself depends on.
///
/// Bodies are transcribed from `java.lang.Math` in JDK 25, including the
/// asymmetry between `max` testing `a`'s sign bit and `min` testing `b`'s.
#[inline]
#[must_use]
pub fn double_max(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == 0x8000_0000_0000_0000 {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// `Math.min(double, double)` — the mirror of [`double_max`].
#[inline]
#[must_use]
pub fn double_min(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == 0x8000_0000_0000_0000 {
        return b;
    }
    if a <= b {
        a
    } else {
        b
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
mod tests;
