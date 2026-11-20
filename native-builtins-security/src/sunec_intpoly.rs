// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! Java body's own contract: `SbrW5Bigint.java` (3,000 random,
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
    #[cfg(target_arch = "x86_64")]
    let _scrub = cratonvm_native_builtins_crypto::scrub::ScrubOnDrop;
    #[cfg(target_arch = "x86_64")]
    if cratonvm_native_builtins_crypto::avx512::ec_enabled()
        && a.iter().chain(b).all(|v| (*v as u64) <= LIMB_MASK as u64)
    {
        return mult_rows(a, b, row_ifma);
    }
    mult_rows(a, b, row)
}

// IFMA truncates its operands to 52 bits. The dispatcher preserves the exact
// Java transcription for negative or oversized limbs (including reflection).
#[cfg(target_arch = "x86_64")]
fn row_ifma(aa: i64, b: &[i64; NUM_LIMBS], cin: i64) -> ([i64; NUM_LIMBS], [i64; NUM_LIMBS]) {
    use cratonvm_native_builtins_crypto::avx512::products52;
    let mut bs = [0u64; 8];
    let mut modulus = [0u64; 8];
    for j in 0..NUM_LIMBS {
        bs[j] = b[j] as u64;
        modulus[j] = MODULUS[j] as u64;
    }
    // SAFETY: the dispatcher checked CPU/OS IFMA support and 52-bit operands.
    let (mut d, mut dd) = unsafe { products52(&[aa as u64; 8], &bs) };
    d[0] = d[0].wrapping_add(cin as u64);
    let n = d[0] & LIMB_MASK as u64;
    // SAFETY: n is masked; all modulus digits fit in 52 bits.
    let (lo, hi) = unsafe { products52(&[n; 8], &modulus) };
    for j in 0..NUM_LIMBS {
        d[j] = d[j].wrapping_add(lo[j]);
        dd[j] = dd[j].wrapping_add(hi[j]);
    }
    (
        std::array::from_fn(|j| d[j] as i64),
        std::array::from_fn(|j| dd[j] as i64),
    )
}

fn mult_rows(
    a: &[i64; NUM_LIMBS],
    b: &[i64; NUM_LIMBS],
    row: impl Fn(i64, &[i64; NUM_LIMBS], i64) -> ([i64; NUM_LIMBS], [i64; NUM_LIMBS]),
) -> [i64; NUM_LIMBS] {
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
                .into());
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
    {
        registry.register(CLASS, "mult", "([J[J[J)V", native_mont_mult, cratonvm_native_api::NativeKind::Intrinsic);
        registry.register(CLASS, "square", "([J[J)V", native_mont_square, cratonvm_native_api::NativeKind::Intrinsic);
    };
}

#[cfg(test)]
mod tests;
