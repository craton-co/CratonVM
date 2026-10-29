// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native intrinsic for the SunEC P-256 Montgomery field multiply/square —
//! `sun.security.util.math.intpoly.MontgomeryIntegerPolynomialP256.{mult,square}`.
//!
//! These two methods dominate EC keygen/sign/verify time: a single keygen pays a
//! one-time generator-table precompute (`Secp256R1GeneratorMontgomeryMultiplier
//! .<clinit>`) that calls them hundreds of thousands of times, and every ECDHE /
//! ECDSA operation of a TLS 1.3 handshake runs its point arithmetic on them.
//! `mult` is an `@IntrinsicCandidate` in JDK 25 (HotSpot's
//! `_intpoly_montgomeryMult_P256`, AVX-512 IFMA); `square` is `mult(a, a, r)`.
//!
//! ## Exact by transcription (SBR wave 5, lane bigint, 2026-09-29)
//!
//! The Rust body is the JDK 25 Java body of `mult`, statement for statement:
//! the same five unrolled rows, the same `Math.unsignedMultiplyHigh` splits at
//! 52 bits, the same `long` arithmetic wrapping, the same final one-modulus
//! subtraction and constant-time mask select. It therefore returns the JDK's
//! limbs for EVERY input, not only for canonical ones.
//!
//! The previous body decoded the limbs to a `num-bigint` value, reduced
//! `a*b*R^-1 mod p` and re-encoded canonically. That equals the Java body only
//! when the inputs are canonical field elements, and the argument for it
//! (`MAX_ADDS = 0`, so every stored element is reduced) did not cover the
//! Java body's own contract: `tools/probes/SbrW5Bigint.java` (3,000 random,
//! any-64-bit and edge-limb calls each for `mult` and `square`) measured its
//! hash different from HotSpot 25.0.4+7 running the Java body (`-Xint`, and C2
//! with the intrinsic disabled) on both methods, and a `BigUint` allocation per
//! call. Exceptions are the Java body's too: `a`, `b` are read in full
//! (`a[0..5)`, then `b[0..5)`) before `r[0..5)` is stored, so a short `a`/`b`
//! throws `Index i out of bounds for length i` with nothing written, a short
//! `r` keeps the stores below its length, and a null array gets HotSpot's
//! helpful-NPE text naming the JDK parameter (`"a"`, `"b"`, `"r"`).

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

const CLASS: &str = "sun/security/util/math/intpoly/MontgomeryIntegerPolynomialP256";
const NUM_LIMBS: usize = 5;
const BITS_PER_LIMB: u32 = 52;
const LIMB_MASK: i64 = (1i64 << BITS_PER_LIMB) - 1;

/// `MontgomeryIntegerPolynomialP256.modulus` (the P-256 prime in 52-bit limbs).
const MODULUS: [i64; NUM_LIMBS] = [
    0x000f_ffff_ffff_ffff,
    0x0000_0fff_ffff_ffff,
    0x0000_0000_0000_0000,
    0x0000_0010_0000_0000,
    0x0000_ffff_ffff_0000,
];

/// `Math.unsignedMultiplyHigh(a, b)`.
#[inline(always)]
fn umulh(a: i64, b: i64) -> i64 {
    (((a as u64 as u128) * (b as u64 as u128)) >> 64) as u64 as i64
}

/// `d = x * y; dd = unsignedMultiplyHigh(x, y) << 12 | (d >>> 52); d &= LIMB_MASK`
/// — the split every row of the Java body opens with.
#[inline(always)]
fn split(x: i64, y: i64) -> (i64, i64) {
    let d = x.wrapping_mul(y);
    let dd = umulh(x, y).wrapping_shl(64 - BITS_PER_LIMB) | ((d as u64) >> BITS_PER_LIMB) as i64;
    (d & LIMB_MASK, dd)
}

/// One row of the Java body: multiply `b` by `aa`, add `cin` into the low
/// digit, and add `n * modulus` where `n = d0 & LIMB_MASK` (row 0 has
/// `cin == 0`, where `n = d0` is the same value).
#[inline(always)]
fn row(aa: i64, b: &[i64; NUM_LIMBS], cin: i64) -> ([i64; NUM_LIMBS], [i64; NUM_LIMBS]) {
    let mut d = [0i64; NUM_LIMBS];
    let mut dd = [0i64; NUM_LIMBS];
    for j in 0..NUM_LIMBS {
        let (lo, hi) = split(aa, b[j]);
        d[j] = lo;
        dd[j] = hi;
    }
    d[0] = d[0].wrapping_add(cin);
    let n = d[0] & LIMB_MASK;
    for j in 0..NUM_LIMBS {
        let nj = n.wrapping_mul(MODULUS[j]);
        dd[j] = dd[j].wrapping_add(
            umulh(n, MODULUS[j]).wrapping_shl(64 - BITS_PER_LIMB)
                | ((nj as u64) >> BITS_PER_LIMB) as i64,
        );
        d[j] = d[j].wrapping_add(nj & LIMB_MASK);
    }
    (d, dd)
}

#[inline(always)]
fn ushr52(x: i64) -> i64 {
    ((x as u64) >> BITS_PER_LIMB) as i64
}

/// `protected void mult(long[] a, long[] b, long[] r)`, JDK 25, transcribed.
/// Returns the five limbs the Java body stores into `r`.
pub fn jdk_mult(a: &[i64; NUM_LIMBS], b: &[i64; NUM_LIMBS]) -> [i64; NUM_LIMBS] {
    // c[k] is the Java body's `ck`.
    let mut c = [0i64; 10];
    // Rows 0..3: c[r+1] += d1 + dd0 + (d0 >>> 52); c[r+2] += d2 + dd1;
    // c[r+3] += d3 + dd2; c[r+4] += d4 + dd3; c[r+5] = dd4. (Row 0's `=` on
    // c1..c5 is `+=` from zero; row r's `c[r+5] =` is the first touch.)
    for r in 0..4 {
        let (d, dd) = row(a[r], b, if r == 0 { 0 } else { c[r] });
        c[r + 1] = c[r + 1].wrapping_add(d[1].wrapping_add(dd[0]).wrapping_add(ushr52(d[0])));
        c[r + 2] = c[r + 2].wrapping_add(d[2].wrapping_add(dd[1]));
        c[r + 3] = c[r + 3].wrapping_add(d[3].wrapping_add(dd[2]));
        c[r + 4] = c[r + 4].wrapping_add(d[4].wrapping_add(dd[3]));
        c[r + 5] = dd[4];
    }
    // Row 4 and the final carry propagation.
    let (d, dd) = row(a[4], b, c[4]);
    c[5] = c[5].wrapping_add(d[1].wrapping_add(dd[0]).wrapping_add(ushr52(d[0])));
    c[6] = c[6].wrapping_add(d[2].wrapping_add(dd[1]).wrapping_add(ushr52(c[5])));
    c[7] = c[7].wrapping_add(d[3].wrapping_add(dd[2]).wrapping_add(ushr52(c[6])));
    c[8] = c[8].wrapping_add(d[4].wrapping_add(dd[3]).wrapping_add(ushr52(c[7])));
    c[9] = dd[4].wrapping_add(ushr52(c[8]));
    c[5] &= LIMB_MASK;
    c[6] &= LIMB_MASK;
    c[7] &= LIMB_MASK;
    c[8] &= LIMB_MASK;
    // Subtract one modulus (signed limbs, arithmetic carries) into e0..e4.
    let mut e0 = c[5].wrapping_sub(MODULUS[0]);
    let mut e1 = c[6]
        .wrapping_sub(MODULUS[1])
        .wrapping_add(e0 >> BITS_PER_LIMB);
    e0 &= LIMB_MASK;
    let mut e2 = c[7]
        .wrapping_sub(MODULUS[2])
        .wrapping_add(e1 >> BITS_PER_LIMB);
    e1 &= LIMB_MASK;
    let mut e3 = c[8]
        .wrapping_sub(MODULUS[3])
        .wrapping_add(e2 >> BITS_PER_LIMB);
    e2 &= LIMB_MASK;
    let e4 = c[9]
        .wrapping_sub(MODULUS[4])
        .wrapping_add(e3 >> BITS_PER_LIMB);
    e3 &= LIMB_MASK;
    let mask = e4 >> 63;
    [
        (c[5] & mask) | (e0 & !mask),
        (c[6] & mask) | (e1 & !mask),
        (c[7] & mask) | (e2 & !mask),
        (c[8] & mask) | (e3 & !mask),
        (c[9] & mask) | (e4 & !mask),
    ]
}

fn npe(what: &str, who: &str) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(format!(
            "Cannot {what} long array because \"{who}\" is null"
        )),
    }
    .into()
}

/// Read `a[0..5)` in order, as the Java body's five `long aaN = a[N]` do.
fn read_limbs(
    ctx: &dyn NativeContext,
    v: Option<&Value>,
    who: &str,
) -> Result<[i64; NUM_LIMBS], MethodCallFailed> {
    let arr = match v {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(npe("load from", who)),
    };
    let len = ctx.array_length(arr);
    if len < NUM_LIMBS {
        return Err(RuntimeError::aioobe(len as i32, len as i32).into());
    }
    let mut out = [0i64; NUM_LIMBS];
    let mut bytes = [0u8; NUM_LIMBS * 8];
    if ctx.read_primitive_array_bytes(arr, 0, &mut bytes) == bytes.len() {
        for (i, slot) in out.iter_mut().enumerate() {
            let mut w = [0u8; 8];
            w.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
            *slot = i64::from_ne_bytes(w);
        }
        return Ok(out);
    }
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Long(v) => *slot = v,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "expected a long[] argument".to_string(),
                }
                .into())
            }
        }
    }
    Ok(out)
}

/// Store `r[0..5)` in order; a short `r` keeps the stores below its length
/// and throws at the first index past it, as the Java body's `r[N] = ...` do.
fn write_limbs(
    ctx: &mut dyn NativeContext,
    v: Option<&Value>,
    limbs: &[i64; NUM_LIMBS],
) -> Result<(), MethodCallFailed> {
    let arr: ObjectRef = match v {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(npe("store to", "r")),
    };
    let len = ctx.array_length(arr);
    if len >= NUM_LIMBS {
        let mut bytes = [0u8; NUM_LIMBS * 8];
        for (i, l) in limbs.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&l.to_ne_bytes());
        }
        if ctx.write_primitive_array_bytes(arr, 0, &bytes) {
            return Ok(());
        }
    }
    for (i, l) in limbs.iter().enumerate() {
        if i >= len {
            return Err(RuntimeError::aioobe(i as i32, len as i32).into());
        }
        ctx.set_array_element(arr, i, Value::Long(*l));
    }
    Ok(())
}

/// `protected void mult(long[] a, long[] b, long[] r)` — args = [this, a, b, r].
fn native_mont_mult(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let av = read_limbs(ctx, args.get(1), "a")?;
    let bv = read_limbs(ctx, args.get(2), "b")?;
    let rv = jdk_mult(&av, &bv);
    write_limbs(ctx, args.get(3), &rv)?;
    Ok(None)
}

/// `protected void square(long[] a, long[] r)` — args = [this, a, r]. The
/// Java body is `mult(a, a, r)`: `a` is read twice (a null or short `a` fails
/// on the first read, named `"a"`), then `r` is stored.
fn native_mont_square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let av = read_limbs(ctx, args.get(1), "a")?;
    let rv = jdk_mult(&av, &av);
    write_limbs(ctx, args.get(2), &rv)?;
    Ok(None)
}

// JDK-ONLY-CLASSIFY: intrinsic. The real
// `MontgomeryIntegerPolynomialP256.{mult,square}` have concrete bytecode;
// `mult` is an `@IntrinsicCandidate` HotSpot replaces, and this body is that
// bytecode transcribed (see the module doc for the measured parity). The two
// registrations state `NativeKind::Intrinsic` themselves (they used to
// inherit it from `native-builtins/src/lib.rs`'s wrapper, which the kind
// being ambient makes fragile — AGENTS.md).
/// Register the P-256 Montgomery field multiply/square intrinsics.
pub fn register_sunec_intpoly_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.with_category(cratonvm_native_api::NativeKind::Intrinsic, |registry| {
        registry.register(CLASS, "mult", "([J[J[J)V", native_mont_mult);
        registry.register(CLASS, "square", "([J[J)V", native_mont_square);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigUint;

    // Limb vectors captured from JDK 25's real
    // MontgomeryIntegerPolynomialP256.{mult,square} (ecprobe_tmp/MontSamples.java).
    // Each MULT entry is (a_limbs, b_limbs, expected_r_limbs).
    #[rustfmt::skip]
    const MULT_VECTORS: &[(&[i64; 5], &[i64; 5], &[i64; 5])] = &[
        // zero * x
        (&[0x0, 0x0, 0x0, 0x0, 0x0],
         &[0xdeadbeefd, 0x300000000000, 0xffffff2152411, 0x1102fffffffff, 0xeadbeeef1524],
         &[0x0, 0x0, 0x0, 0x0, 0x0]),
        // one(mont) * x
        (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac],
         &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac]),
        // x * one(mont)
        (&[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1]),
        // (p-1) * (p-1)
        (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
        // random 1
        (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
         &[0xfdeef74613ef0, 0xd1abc768a0775, 0x7492792376809, 0x871743f7451f6, 0x51d467691430],
         &[0x5d345a7ca4667, 0xb48fc3b761b32, 0x3eb9eb427c5d3, 0xf10a7242677fc, 0x1c5bea89843c]),
        // random 2
        (&[0xfffee789abcc6, 0x54aaaaaab7fff, 0x21876543, 0x431f555555400, 0x789abcbf8765],
         &[0xaaa899999996e, 0x5eccccccaaaaa, 0x5555575555555, 0x77e5333333755, 0xeeeeeee57777],
         &[0xca82d7fcf7571, 0x251b27abfacdd, 0x1eb83d0c8db0e, 0x8632fde04ab05, 0xb44cc347d916]),
    ];

    // SQUARE entries (a_limbs, expected_r_limbs) from the real JDK square().
    #[rustfmt::skip]
    const SQUARE_VECTORS: &[(&[i64; 5], &[i64; 5])] = &[
        (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
         &[0x4f4424402354b, 0xb23fcdfd72be7, 0xf1bea8600adff, 0x68720d8f836c7, 0x7366dace0f4b]),
        (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
        (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
    ];

    // (a, b, r) from HotSpot 25.0.4+7 `-Xint` (the Java body), out-of-range
    // limbs: `tools/probes/SbrW5Bigint.java limbs`, 2026-09-29.
    #[rustfmt::skip]
    const HOTSPOT_EDGE_VECTORS: &[(&[i64; 5], &[i64; 5], &[i64; 5])] = &[
        (&[-1, -1, -1, -1, -1], &[-1, -1, -1, -1, -1],
         &[2903124356194211, 3672921276709762, 3375846714481153, 1410745367181051, 233113644889080]),
        (&[i64::MIN, i64::MIN, i64::MIN, i64::MIN, i64::MIN], &[1, 2, 3, 4, 5],
         &[844424795945008, 846073794949328, 3660823158874496, 2251802764921600, 17592186051841]),
        (&[4503599627370496, 4503599627370496, 4503599627370496, 4503599627370496, 4503599627370496],
         &[4503599627370495, 4503599627370495, 4503599627370495, 4503599627370495, 4503599627370495],
         &[4204395025727487, 4221986406465536, 4220956151185664, 4221024868565248, 4502491525808386]),
        (&[4503599627370495, 17592186044415, 0, 68719476736, 281474976645120],
         &[4503599627370495, 17592186044415, 0, 68719476736, 281474976645120],
         &[0, 0, 0, 0, 0]),
        (&[-1, 0, -1, 0, -1], &[i64::MAX, 12345, i64::MAX, -98765, 7],
         &[2445590755185333, 1147828580619821, 1969976928259022, 3121627012799220, 21500584191914]),
    ];

    /// An independent, value-level reference: `a*b*2^-260 mod p` through
    /// `num-bigint`, canonical limbs out. For canonical inputs the Java body
    /// is fully reduced, so the two must agree limb for limb.
    fn value_mult(a: &[i64; 5], b: &[i64; 5]) -> [i64; 5] {
        let p = BigUint::parse_bytes(
            b"ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
            16,
        )
        .unwrap();
        let dec = |l: &[i64; 5]| {
            let mut x = BigUint::from(0u8);
            for i in (0..5).rev() {
                x = (x << 52u32) + BigUint::from(l[i] as u64);
            }
            x
        };
        let r = (BigUint::from(1u8) << 260u32) % &p;
        let r_inv = r.modpow(&(&p - 2u32), &p);
        let mut v = dec(a) * dec(b) % &p * r_inv % &p;
        let mask = BigUint::from(LIMB_MASK as u64);
        let mut out = [0i64; 5];
        for slot in out.iter_mut() {
            *slot = (&v & &mask).iter_u64_digits().next().unwrap_or(0) as i64;
            v >>= 52u32;
        }
        out
    }

    #[test]
    fn mult_matches_jdk_byte_for_byte() {
        for (a, b, expect) in MULT_VECTORS {
            assert_eq!(
                &jdk_mult(a, b),
                *expect,
                "mult mismatch for a={a:x?} b={b:x?}"
            );
        }
    }

    #[test]
    fn square_matches_jdk_and_equals_mult_self() {
        for (a, expect) in SQUARE_VECTORS {
            assert_eq!(&jdk_mult(a, a), *expect, "square mismatch for a={a:x?}");
        }
    }

    /// Random canonical field elements: the transcription agrees with the
    /// value-level reference (an independent code path) and stays canonical.
    #[test]
    fn canonical_inputs_agree_with_the_value_reference() {
        let p_limbs = MODULUS;
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let lt_p = |x: &[i64; 5]| {
            for i in (0..5).rev() {
                if x[i] != p_limbs[i] {
                    return x[i] < p_limbs[i];
                }
            }
            false
        };
        let mut checked = 0;
        while checked < 2000 {
            let mut a = [0i64; 5];
            let mut b = [0i64; 5];
            for i in 0..5 {
                a[i] = (next() as i64) & LIMB_MASK;
                b[i] = (next() as i64) & LIMB_MASK;
            }
            a[4] &= 0xffff_ffff_ffff; // below 2^256
            b[4] &= 0xffff_ffff_ffff;
            if !lt_p(&a) || !lt_p(&b) {
                continue;
            }
            let got = jdk_mult(&a, &b);
            assert_eq!(got, value_mult(&a, &b), "a={a:x?} b={b:x?}");
            for limb in got {
                assert!(
                    (0..(1i64 << 52)).contains(&limb),
                    "non-canonical limb {limb:#x}"
                );
            }
            checked += 1;
        }
    }

    /// Out-of-range limbs are where the old value-level body diverged: the
    /// Java body is plain wrapping `long` arithmetic there, not a field
    /// operation. The expected limbs are HotSpot 25.0.4+7's (`-Xint`, the Java
    /// body) for `MontgomeryIntegerPolynomialP256.ONE.mult`, printed by
    /// `tools/probes/SbrW5Bigint.java limbs`.
    #[test]
    fn out_of_range_limbs_follow_the_java_arithmetic() {
        for (a, b, expect) in HOTSPOT_EDGE_VECTORS {
            assert_eq!(&jdk_mult(a, b), *expect, "a={a:x?} b={b:x?}");
        }
    }
}
