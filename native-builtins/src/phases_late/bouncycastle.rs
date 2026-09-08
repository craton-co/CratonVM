// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Bouncy Castle kernels: EC field/point arithmetic (Fp/F2m/SecT), block and stream ciphers, digests, KDFs and PRNGs.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// BigInteger extensions — uses existing bi_read/bi_alloc plus the
// arbitrary-precision decimal-string helpers in lib.rs. The previous
// implementation funnelled everything through `bi_read().parse::<i128>()
// .unwrap_or(0)`, which silently produced 0 for any value > 128 bits — see
// commit 958baae for the matching intValue/longValue fix. The pattern below
// preserves full precision by operating on the decimal string `bi_read`
// already returns for any BigInteger (whether the synthetic value-string
// layout or the real-JDK signum/mag[I] layout).
// =============================================================================

/// Native fast-path for BouncyCastle's RSA-keygen small-factor prime
/// pre-screen `org.bouncycastle.math.Primes.implHasAnySmallFactors`.
///
/// The Java method computes `x mod m` — via `BigInteger.valueOf(m)`,
/// `BigInteger.mod`, then `intValue()` — for ten ~32-bit moduli, each a
/// product of consecutive small primes, and tests the remainder against every
/// prime in the group. (The `org/bouncycastle/*` JIT ban this was written under
/// is long gone — no ban list names the package today — but the allocation cost
/// below is what motivates the intrinsic and does not depend on it.)
/// ~10 BigInteger allocations + 10 limb-division calls per
/// candidate, over hundreds of candidates per RSA prime, which dominates
/// `RSAKeyPairGenerator.chooseRandomPrime` (see `RSATest.test_CVE_2017_15361`,
/// the documented RSA non-finish — `bug-bc-crypto-regression-timeout.md`). This intrinsic reads the candidate's magnitude
/// once and computes each `x mod m` with a single Horner pass over the limbs
/// (zero allocation), returning the method's exact boolean result.
///
/// Byte-exact with the BC source: `m` is recomputed as the product of each
/// group's primes (all products < 2^32), so the trial-divisor set is
/// identical. Only the private, pure `implHasAnySmallFactors` leaf is replaced;
/// the public `hasAnySmallFactors` wrapper (and its `checkCandidate`, which
/// guarantees a positive candidate ≥ 2) still runs as real bytecode. Tagged
/// `Intrinsic`, not a stub.
/// Consecutive small-prime groups, identical to the moduli in
/// org.bouncycastle.math.Primes.implHasAnySmallFactors (primes 2..211). Each
/// group's modulus `m` = product of its primes (every product < 2^32).
pub(crate) const BC_SMALL_FACTOR_GROUPS: [&[u32]; 10] = [
    &[2, 3, 5, 7, 11, 13, 17, 19, 23],
    &[29, 31, 37, 41, 43],
    &[47, 53, 59, 61, 67],
    &[71, 73, 79, 83],
    &[89, 97, 101, 103],
    &[107, 109, 113, 127],
    &[131, 137, 139, 149],
    &[151, 157, 163, 167],
    &[173, 179, 181, 191],
    &[193, 197, 199, 211],
];

/// Core of `Primes.implHasAnySmallFactors`: true iff the integer with
/// little-endian base-2^32 magnitude `mag` (sign `negative`) is divisible by
/// any prime in [2, 211]. Pure / allocation-free; see
/// [`register_bc_primes_small_factors`].
pub(crate) fn bc_has_any_small_factors(mag: &[u32], negative: bool) -> bool {
    for primes in BC_SMALL_FACTOR_GROUPS {
        // Product of the group's primes == the Java `int m`.
        let m: u64 = primes.iter().map(|&p| p as u64).product();
        // x mod m via Horner over the limbs, most-significant first.
        // rem < m <= ~1.6e9 and limb < 2^32, so (rem<<32)|limb < 2^63.
        let mut rem: u64 = 0;
        for &limb in mag.iter().rev() {
            rem = ((rem << 32) | limb as u64) % m;
        }
        let mut r32 = rem as u32;
        // BigInteger.mod returns a non-negative remainder; for the
        // (contract-guaranteed-absent but defensively handled) negative
        // candidate, fold |x| mod m into [0, m). r32 < m here.
        if negative && r32 != 0 {
            r32 = (m as u32) - r32;
        }
        if primes.iter().any(|&p| r32 % p == 0) {
            return true;
        }
    }
    false
}

/// The odd primes 3..=743 — the exact prime factors of
/// `org.bouncycastle.util.BigIntegers.SMALL_PRIMES_PRODUCT` (verified: their
/// product equals the class's hex literal; 2 is excluded because the candidate
/// is forced odd first). Used by [`register_bc_util_small_factors`].
pub(crate) const BC_ODD_SMALL_PRIMES: [u32; 131] = [
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
    101, 103, 107, 109, 113, 127, 131, 137, 139, 149, 151, 157, 163, 167, 173, 179, 181, 191, 193,
    197, 199, 211, 223, 227, 229, 233, 239, 241, 251, 257, 263, 269, 271, 277, 281, 283, 293, 307,
    311, 313, 317, 331, 337, 347, 349, 353, 359, 367, 373, 379, 383, 389, 397, 401, 409, 419, 421,
    431, 433, 439, 443, 449, 457, 461, 463, 467, 479, 487, 491, 499, 503, 509, 521, 523, 541, 547,
    557, 563, 569, 571, 577, 587, 593, 599, 601, 607, 613, 617, 619, 631, 641, 643, 647, 653, 659,
    661, 673, 677, 683, 691, 701, 709, 719, 727, 733, 739, 743,
];

/// `x mod p` for a single 32-bit prime `p`, via Horner over little-endian
/// base-2^32 magnitude limbs. Returns the non-negative remainder of |x| mod p.
#[inline]
pub(crate) fn mag_mod_u32(mag: &[u32], p: u32) -> u32 {
    let p = p as u64;
    let mut rem: u64 = 0;
    for &limb in mag.iter().rev() {
        rem = ((rem << 32) | limb as u64) % p;
    }
    rem as u32
}

/// Core of `util.BigIntegers.hasAnySmallFactors`: true iff `x` is divisible by
/// any prime ≤ 743 (i.e. shares a factor with `SMALL_PRIMES_PRODUCT`, or is
/// even). Divisibility is sign-invariant, so the magnitude suffices.
pub(crate) fn bc_util_has_any_small_factors(mag: &[u32]) -> bool {
    // x even? (low limb's bit 0; empty magnitude == 0 == even)
    if mag.first().copied().unwrap_or(0) & 1 == 0 {
        return true;
    }
    BC_ODD_SMALL_PRIMES
        .iter()
        .any(|&p| mag_mod_u32(mag, p) == 0)
}

pub(crate) fn bc_bigint_small(v: u32) -> crate::bigint::BigInt {
    if v == 0 {
        crate::bigint::BigInt::zero()
    } else {
        crate::bigint::BigInt::from_le_words(false, vec![v])
    }
}

pub(crate) fn bc_primes_candidate_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
    name: &str,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let Some(Value::Object(Some(obj))) = args.get(idx) else {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{name}' must be non-null and >= 2"),
        }
        .into());
    };
    let n = bi_read_int(ctx, *obj);
    if n.signum() < 1 || n.bit_length() < 2 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{name}' must be non-null and >= 2"),
        }
        .into());
    }
    Ok(n)
}

pub(crate) fn bc_mr_probable_prime_to_base(
    candidate: &crate::bigint::BigInt,
    base: &crate::bigint::BigInt,
) -> bool {
    if candidate.bit_length() == 2 {
        return true;
    }

    let one = bc_bigint_small(1);
    let two = bc_bigint_small(2);
    let w_sub_one = candidate.sub(&one);

    let mut a = 0u32;
    while !w_sub_one.test_bit(a) {
        a += 1;
    }
    let m = w_sub_one.shr(a);

    let mut z = base.modpow(&m, candidate);
    if z.cmp(&one) == std::cmp::Ordering::Equal || z.cmp(&w_sub_one) == std::cmp::Ordering::Equal {
        return true;
    }

    for _ in 1..a {
        z = z.modpow(&two, candidate);
        if z.cmp(&w_sub_one) == std::cmp::Ordering::Equal {
            return true;
        }
        if z.cmp(&one) == std::cmp::Ordering::Equal {
            return false;
        }
    }

    false
}

pub(crate) fn bc_big_integers_mod_odd_inverse(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    variable_time: bool,
) -> MethodCallResult {
    let modulus = bi_read_int(ctx, obj_arg(args, 0)?);
    if !modulus.test_bit(0) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "'M' must be odd".into(),
        }
        .into());
    }
    if modulus.signum() != 1 {
        return Err(RuntimeError::ArithmeticException {
            message: "BigInteger: modulus not positive".into(),
        }
        .into());
    }
    if modulus.bit_length() == 1 {
        return Ok(Some(Value::Object(Some(bi_alloc_int(
            ctx,
            &crate::bigint::BigInt::zero(),
        )?))));
    }

    let mut x = bi_read_int(ctx, obj_arg(args, 1)?);
    if x.is_neg() || x.bit_length() > modulus.bit_length() {
        x = x.modulo(&modulus);
    }
    if variable_time && x.bit_length() == 1 && x.test_bit(0) {
        return Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &x)?))));
    }

    let inv = x.mod_inverse(&modulus).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::ArithmeticException {
            message: "BigInteger not invertible.".into(),
        })
    })?;
    Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &inv)?))))
}

pub(crate) fn bc_long_array_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_long_array_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, "m_ints") {
        Value::Object(Some(arr)) => Ok(arr),
        _ => Err(bc_long_array_bad_state("LongArray: malformed m_ints")),
    }
}

pub(crate) fn bc_read_long_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u64> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(match ctx.get_array_element(arr, i) {
            Value::Long(v) => v as u64,
            Value::Int(v) => v as u32 as u64,
            _ => 0,
        });
    }
    out
}

pub(crate) fn bc_read_longarray_value(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<Vec<u64>, MethodCallFailed> {
    Ok(bc_read_long_array(ctx, bc_long_array_field(ctx, obj)?))
}

pub(crate) fn bc_read_i32_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i32> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            Value::Long(v) => v as i32,
            _ => 0,
        });
    }
    out
}

pub(crate) fn bc_longarray_ks(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<Vec<usize>, MethodCallFailed> {
    let arr = obj_arg(args, idx)?;
    let mut ks = Vec::new();
    for k in bc_read_i32_array(ctx, arr) {
        if k <= 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "LongArray: invalid reduction polynomial".into(),
            }
            .into());
        }
        ks.push(k as usize);
    }
    Ok(ks)
}

pub(crate) fn bc_trim_poly(x: &mut Vec<u64>) {
    while x.last().copied() == Some(0) {
        x.pop();
    }
}

pub(crate) fn bc_poly_degree(x: &[u64]) -> usize {
    for (i, &word) in x.iter().enumerate().rev() {
        if word != 0 {
            return i * 64 + (64 - word.leading_zeros() as usize);
        }
    }
    0
}

pub(crate) fn bc_poly_test_bit(x: &[u64], bit: usize) -> bool {
    x.get(bit >> 6)
        .map(|word| (word & (1u64 << (bit & 63))) != 0)
        .unwrap_or(false)
}

pub(crate) fn bc_poly_flip_bit(x: &mut Vec<u64>, bit: usize) {
    let word = bit >> 6;
    if word >= x.len() {
        x.resize(word + 1, 0);
    }
    x[word] ^= 1u64 << (bit & 63);
}

pub(crate) fn bc_poly_xor_shift(dst: &mut Vec<u64>, src: &[u64], shift: usize) {
    if src.is_empty() {
        return;
    }
    let word_shift = shift >> 6;
    let bit_shift = shift & 63;
    let need = word_shift + src.len() + usize::from(bit_shift != 0);
    if dst.len() < need {
        dst.resize(need, 0);
    }
    if bit_shift == 0 {
        for (i, &word) in src.iter().enumerate() {
            dst[word_shift + i] ^= word;
        }
    } else {
        for (i, &word) in src.iter().enumerate() {
            dst[word_shift + i] ^= word << bit_shift;
            dst[word_shift + i + 1] ^= word >> (64 - bit_shift);
        }
    }
}

pub(crate) fn bc_poly_mask_to_m(x: &mut Vec<u64>, m: usize) {
    let len = (m + 63) >> 6;
    x.truncate(len);
    if m & 63 != 0 {
        if x.len() == len {
            x[len - 1] &= (1u64 << (m & 63)) - 1;
        }
    }
    bc_trim_poly(x);
}

pub(crate) fn bc_poly_modulus(m: usize, ks: &[usize]) -> Vec<u64> {
    let mut f = Vec::new();
    bc_poly_flip_bit(&mut f, 0);
    bc_poly_flip_bit(&mut f, m);
    for &k in ks {
        bc_poly_flip_bit(&mut f, k);
    }
    f
}

pub(crate) fn bc_poly_reduce(mut x: Vec<u64>, m: usize, ks: &[usize]) -> Vec<u64> {
    if m == 0 {
        x.clear();
        return x;
    }

    let mut d = bc_poly_degree(&x);
    while d > m {
        let bit = d - 1;
        if bc_poly_test_bit(&x, bit) {
            bc_poly_flip_bit(&mut x, bit);
            let n = bit - m;
            bc_poly_flip_bit(&mut x, n);
            for &k in ks {
                bc_poly_flip_bit(&mut x, n + k);
            }
        }
        d = bc_poly_degree(&x);
    }
    bc_poly_mask_to_m(&mut x, m);
    x
}

pub(crate) fn bc_poly_mul_raw(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (small, large) = if bc_poly_degree(a) <= bc_poly_degree(b) {
        (a, b)
    } else {
        (b, a)
    };
    if small.is_empty() || large.is_empty() {
        return Vec::new();
    }

    let mut out = vec![0u64; small.len() + large.len()];
    for (word_idx, &word) in small.iter().enumerate() {
        let mut w = word;
        while w != 0 {
            let bit = w.trailing_zeros() as usize;
            bc_poly_xor_shift(&mut out, large, word_idx * 64 + bit);
            w &= w - 1;
        }
    }
    bc_trim_poly(&mut out);
    out
}

pub(crate) fn bc_poly_square_raw(a: &[u64]) -> Vec<u64> {
    if a.is_empty() {
        return Vec::new();
    }

    let mut out = vec![0u64; a.len() * 2];
    for (word_idx, &word) in a.iter().enumerate() {
        let mut w = word;
        while w != 0 {
            let bit = w.trailing_zeros() as usize;
            bc_poly_flip_bit(&mut out, word_idx * 128 + bit * 2);
            w &= w - 1;
        }
    }
    bc_trim_poly(&mut out);
    out
}

pub(crate) fn bc_poly_mod_square_n(mut a: Vec<u64>, n: i32, m: usize, ks: &[usize]) -> Vec<u64> {
    let rounds = n.max(0) as usize;
    for _ in 0..rounds {
        a = bc_poly_reduce(bc_poly_square_raw(&a), m, ks);
    }
    a
}

pub(crate) fn bc_poly_inverse(a: &[u64], m: usize, ks: &[usize]) -> Option<Vec<u64>> {
    let mut u = bc_poly_reduce(a.to_vec(), m, ks);
    match bc_poly_degree(&u) {
        0 => return None,
        1 => return Some(u),
        _ => {}
    }

    let mut v = bc_poly_modulus(m, ks);
    let mut g1 = vec![1u64];
    let mut g2 = Vec::new();

    while bc_poly_degree(&u) != 1 {
        let mut du = bc_poly_degree(&u);
        let mut dv = bc_poly_degree(&v);
        if du == 0 {
            return None;
        }
        if du < dv {
            std::mem::swap(&mut u, &mut v);
            std::mem::swap(&mut g1, &mut g2);
            std::mem::swap(&mut du, &mut dv);
        }
        let shift = du - dv;
        bc_poly_xor_shift(&mut u, &v, shift);
        bc_poly_xor_shift(&mut g1, &g2, shift);
        bc_trim_poly(&mut u);
        bc_trim_poly(&mut g1);
    }

    Some(bc_poly_reduce(g1, m, ks))
}

/// The smallest `m_ints` BouncyCastle's `LongArray` ever carries.
///
/// `bc_trim_poly` drops trailing zero words, so the ZERO polynomial trims to an
/// empty slice — and an empty `long[]` is not a value `LongArray` accepts.
/// `isOne()` reads `a[0]` with no length test (`isZero()` loops and so survives
/// one, which is why this stayed hidden), and the class's own
/// `LongArray(BigInteger)` spells the intended representation out: a zero
/// bigInt becomes `new long[]{ 0L }`, never `new long[0]`. A native handing
/// back the empty array therefore builds a `LongArray` no BouncyCastle
/// constructor could have produced, and the next `isOne()` on it raises
/// `ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0` —
/// `GeneralKeyTest.testDstu4145`, via `DSTU4145PointEncoder.encodePoint`.
const LONG_ARRAY_MIN_WORDS: usize = 1;

pub(crate) fn bc_alloc_long_array(
    ctx: &mut dyn NativeContext,
    words: &[u64],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Long,
        words.len().max(LONG_ARRAY_MIN_WORDS),
    );
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(word as i64));
    }
    let arr_pin = ctx.pin_native_root(arr);
    let result = match ctx.new_object("org/bouncycastle/math/ec/LongArray")? {
        Some(Value::Object(Some(obj))) => {
            ctx.set_field_by_name(
                obj,
                "m_ints",
                Value::Object(Some(ctx.read_native_pin(arr_pin, arr))),
            );
            Ok(obj)
        }
        _ => Err(bc_long_array_bad_state("LongArray: allocation failed")),
    };
    ctx.unpin_native_roots(arr_pin);
    result
}

pub(crate) fn bc_longarray_return(ctx: &mut dyn NativeContext, words: &[u64]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_alloc_long_array(ctx, words)?))))
}

pub(crate) fn bc_poly_add(mut a: Vec<u64>, b: &[u64]) -> Vec<u64> {
    bc_poly_xor_shift(&mut a, b, 0);
    bc_trim_poly(&mut a);
    a
}

pub(crate) fn bc_longarray_set_value(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    words: &[u64],
) -> Result<(), MethodCallFailed> {
    // Same floor as `bc_alloc_long_array`; see `LONG_ARRAY_MIN_WORDS`.
    let arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Long,
        words.len().max(LONG_ARRAY_MIN_WORDS),
    );
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(word as i64));
    }
    ctx.set_field_by_name(this, "m_ints", Value::Object(Some(arr)));
    Ok(())
}

pub(crate) fn bc_longarray_this_or_other(
    ctx: &dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let a_words = bc_read_longarray_value(ctx, a)?;
    if bc_poly_degree(&a_words) == 0 {
        return Ok(Some(a));
    }
    let b_words = bc_read_longarray_value(ctx, b)?;
    if bc_poly_degree(&b_words) == 0 {
        return Ok(Some(b));
    }
    Ok(None)
}

pub(crate) fn register_bc_long_array(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/math/ec/LongArray";

    r.register(
        cls,
        "modReduce",
        "(I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = match args.get(1) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "LongArray: invalid field degree".into(),
                    }
                    .into())
                }
            };
            let ks = bc_longarray_ks(ctx, args, 2)?;
            let x = bc_read_longarray_value(ctx, this)?;
            bc_longarray_return(ctx, &bc_poly_reduce(x, m, &ks))
        },
    );

    r.register(cls, "reduce", "(I[I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let m = match args.get(1) {
            Some(Value::Int(v)) if *v > 0 => *v as usize,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "LongArray: invalid field degree".into(),
                }
                .into())
            }
        };
        let ks = bc_longarray_ks(ctx, args, 2)?;
        let x = bc_read_longarray_value(ctx, this)?;
        let reduced = bc_poly_reduce(x, m, &ks);
        bc_longarray_set_value(ctx, this, &reduced)?;
        Ok(None)
    });

    r.register(
        cls,
        "modMultiply",
        "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            if let Some(existing) = bc_longarray_this_or_other(ctx, this, other)? {
                return Ok(Some(Value::Object(Some(existing))));
            }
            let m = match args.get(2) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "LongArray: invalid field degree".into(),
                    }
                    .into())
                }
            };
            let ks = bc_longarray_ks(ctx, args, 3)?;
            let a = bc_read_longarray_value(ctx, this)?;
            let b = bc_read_longarray_value(ctx, other)?;
            let product = bc_poly_mul_raw(&a, &b);
            bc_longarray_return(ctx, &bc_poly_reduce(product, m, &ks))
        },
    );

    r.register(
        cls,
        "multiply",
        "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            if let Some(existing) = bc_longarray_this_or_other(ctx, this, other)? {
                return Ok(Some(Value::Object(Some(existing))));
            }
            let a = bc_read_longarray_value(ctx, this)?;
            let b = bc_read_longarray_value(ctx, other)?;
            bc_longarray_return(ctx, &bc_poly_mul_raw(&a, &b))
        },
    );

    r.register(
        cls,
        "modSquare",
        "(I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let a = bc_read_longarray_value(ctx, this)?;
            if bc_poly_degree(&a) == 0 {
                return Ok(Some(Value::Object(Some(this))));
            }
            let m = match args.get(1) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "LongArray: invalid field degree".into(),
                    }
                    .into())
                }
            };
            let ks = bc_longarray_ks(ctx, args, 2)?;
            bc_longarray_return(ctx, &bc_poly_reduce(bc_poly_square_raw(&a), m, &ks))
        },
    );

    r.register(
        cls,
        "square",
        "(I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let a = bc_read_longarray_value(ctx, this)?;
            if bc_poly_degree(&a) == 0 {
                return Ok(Some(Value::Object(Some(this))));
            }
            bc_longarray_return(ctx, &bc_poly_square_raw(&a))
        },
    );

    r.register(
        cls,
        "modSquareN",
        "(II[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let a = bc_read_longarray_value(ctx, this)?;
            if bc_poly_degree(&a) == 0 {
                return Ok(Some(Value::Object(Some(this))));
            }
            let n = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let m = match args.get(2) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "LongArray: invalid field degree".into(),
                    }
                    .into())
                }
            };
            let ks = bc_longarray_ks(ctx, args, 3)?;
            bc_longarray_return(ctx, &bc_poly_mod_square_n(a, n, m, &ks))
        },
    );

    r.register(
        cls,
        "modInverse",
        "(I[I)Lorg/bouncycastle/math/ec/LongArray;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let a = bc_read_longarray_value(ctx, this)?;
            let degree = bc_poly_degree(&a);
            if degree == 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "LongArray: zero is not invertible".into(),
                }
                .into());
            }
            if degree == 1 {
                return Ok(Some(Value::Object(Some(this))));
            }
            let m = match args.get(1) {
                Some(Value::Int(v)) if *v > 0 => *v as usize,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "LongArray: invalid field degree".into(),
                    }
                    .into())
                }
            };
            let ks = bc_longarray_ks(ctx, args, 2)?;
            let inv = bc_poly_inverse(&a, m, &ks).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "LongArray: polynomial not invertible".into(),
                })
            })?;
            bc_longarray_return(ctx, &inv)
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_f2m_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_f2m_ks_from_obj(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
) -> Result<Vec<usize>, MethodCallFailed> {
    let mut ks = Vec::new();
    for k in bc_read_i32_array(ctx, arr) {
        if k <= 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "ECFieldElement.F2m: invalid reduction polynomial".into(),
            }
            .into());
        }
        ks.push(k as usize);
    }
    Ok(ks)
}

pub(crate) fn bc_f2m_read_this(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(i32, ObjectRef, Vec<usize>, Vec<u64>), MethodCallFailed> {
    let m = match ctx.get_field_by_name(this, "m") {
        Value::Int(v) if v > 0 => v,
        _ => return Err(bc_f2m_bad_state("ECFieldElement.F2m: invalid field degree")),
    };
    let ks_obj = match ctx.get_field_by_name(this, "ks") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(bc_f2m_bad_state(
                "ECFieldElement.F2m: missing reduction polynomial",
            ))
        }
    };
    let x_obj = match ctx.get_field_by_name(this, "x") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_f2m_bad_state("ECFieldElement.F2m: missing LongArray")),
    };
    Ok((
        m,
        ks_obj,
        bc_f2m_ks_from_obj(ctx, ks_obj)?,
        bc_read_longarray_value(ctx, x_obj)?,
    ))
}

pub(crate) fn bc_f2m_arg_value(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<Vec<u64>, MethodCallFailed> {
    let obj = obj_arg(args, idx)?;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        != Some("org/bouncycastle/math/ec/ECFieldElement$F2m")
    {
        return Err(RuntimeError::IllegalArgumentException {
            message: "ECFieldElement.F2m: incompatible field element".into(),
        }
        .into());
    }
    let x_obj = match ctx.get_field_by_name(obj, "x") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_f2m_bad_state("ECFieldElement.F2m: missing LongArray")),
    };
    Ok(bc_read_longarray_value(ctx, x_obj)?)
}

pub(crate) fn bc_f2m_alloc(
    ctx: &mut dyn NativeContext,
    m: i32,
    ks_obj: ObjectRef,
    x: &[u64],
) -> Result<ObjectRef, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(ks_obj);
    let x_obj = match bc_alloc_long_array(ctx, x) {
        Ok(obj) => obj,
        Err(e) => {
            ctx.unpin_native_roots(base_pin);
            return Err(e);
        }
    };
    let x_pin = ctx.pin_native_root(x_obj);
    let result = match ctx.new_object("org/bouncycastle/math/ec/ECFieldElement$F2m") {
        Ok(Some(Value::Object(Some(obj)))) => {
            ctx.set_field_by_name(obj, "m", Value::Int(m));
            ctx.set_field_by_name(
                obj,
                "representation",
                Value::Int(if ctx.array_length(ks_obj) == 1 { 2 } else { 3 }),
            );
            ctx.set_field_by_name(
                obj,
                "ks",
                Value::Object(Some(ctx.read_native_pin(base_pin, ks_obj))),
            );
            ctx.set_field_by_name(
                obj,
                "x",
                Value::Object(Some(ctx.read_native_pin(x_pin, x_obj))),
            );
            Ok(obj)
        }
        Ok(_) => Err(bc_f2m_bad_state("ECFieldElement.F2m: allocation failed")),
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(base_pin);
    result
}

pub(crate) fn bc_f2m_return(
    ctx: &mut dyn NativeContext,
    m: i32,
    ks_obj: ObjectRef,
    x: Vec<u64>,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_f2m_alloc(ctx, m, ks_obj, &x)?))))
}

pub(crate) fn register_bc_f2m_field_element(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/math/ec/ECFieldElement$F2m";

    r.register(
        cls,
        "add",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, _, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            bc_f2m_return(ctx, m, ks_obj, bc_poly_add(x, &rhs))
        },
    );

    r.register(
        cls,
        "subtract",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, _, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            bc_f2m_return(ctx, m, ks_obj, bc_poly_add(x, &rhs))
        },
    );

    r.register(
        cls,
        "addOne",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, _, mut x) = bc_f2m_read_this(ctx, this)?;
            bc_poly_flip_bit(&mut x, 0);
            bc_trim_poly(&mut x);
            bc_f2m_return(ctx, m, ks_obj, x)
        },
    );

    r.register(
        cls,
        "multiply",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            let z = bc_poly_reduce(bc_poly_mul_raw(&x, &rhs), m as usize, &ks);
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "multiplyPlusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, a) = bc_f2m_read_this(ctx, this)?;
            let b = bc_f2m_arg_value(ctx, args, 1)?;
            let x = bc_f2m_arg_value(ctx, args, 2)?;
            let y = bc_f2m_arg_value(ctx, args, 3)?;
            let z = bc_poly_reduce(
                bc_poly_add(bc_poly_mul_raw(&a, &b), &bc_poly_mul_raw(&x, &y)),
                m as usize,
                &ks,
            );
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "multiplyMinusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, a) = bc_f2m_read_this(ctx, this)?;
            let b = bc_f2m_arg_value(ctx, args, 1)?;
            let x = bc_f2m_arg_value(ctx, args, 2)?;
            let y = bc_f2m_arg_value(ctx, args, 3)?;
            let z = bc_poly_reduce(
                bc_poly_add(bc_poly_mul_raw(&a, &b), &bc_poly_mul_raw(&x, &y)),
                m as usize,
                &ks,
            );
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "divide",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            let inv = bc_poly_inverse(&rhs, m as usize, &ks).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "LongArray: polynomial not invertible".into(),
                })
            })?;
            let z = bc_poly_reduce(bc_poly_mul_raw(&x, &inv), m as usize, &ks);
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |_ctx, args| Ok(Some(Value::Object(Some(obj_arg(args, 0)?)))),
    );

    r.register(
        cls,
        "square",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            let z = bc_poly_reduce(bc_poly_square_raw(&x), m as usize, &ks);
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "squarePlusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, a) = bc_f2m_read_this(ctx, this)?;
            let x = bc_f2m_arg_value(ctx, args, 1)?;
            let y = bc_f2m_arg_value(ctx, args, 2)?;
            let z = bc_poly_reduce(
                bc_poly_add(bc_poly_square_raw(&a), &bc_poly_mul_raw(&x, &y)),
                m as usize,
                &ks,
            );
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "squareMinusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, a) = bc_f2m_read_this(ctx, this)?;
            let x = bc_f2m_arg_value(ctx, args, 1)?;
            let y = bc_f2m_arg_value(ctx, args, 2)?;
            let z = bc_poly_reduce(
                bc_poly_add(bc_poly_square_raw(&a), &bc_poly_mul_raw(&x, &y)),
                m as usize,
                &ks,
            );
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    r.register(
        cls,
        "squarePow",
        "(I)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pow = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            if pow < 1 {
                return Ok(Some(Value::Object(Some(this))));
            }
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            bc_f2m_return(
                ctx,
                m,
                ks_obj,
                bc_poly_mod_square_n(x, pow, m as usize, &ks),
            )
        },
    );

    r.register(
        cls,
        "invert",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            let inv = bc_poly_inverse(&x, m as usize, &ks).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "LongArray: polynomial not invertible".into(),
                })
            })?;
            bc_f2m_return(ctx, m, ks_obj, inv)
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) struct BcF2mPointData {
    curve: ObjectRef,
    coord: i32,
    m: i32,
    ks_obj: ObjectRef,
    ks: Vec<usize>,
    x: Vec<u64>,
    y: Vec<u64>,
    z: Vec<u64>,
}

pub(crate) fn bc_f2m_add2(a: &[u64], b: &[u64]) -> Vec<u64> {
    bc_poly_add(a.to_vec(), b)
}

pub(crate) fn bc_f2m_add_many(first: &[u64], rest: &[&[u64]]) -> Vec<u64> {
    let mut out = first.to_vec();
    for part in rest {
        out = bc_poly_add(out, part);
    }
    out
}

pub(crate) fn bc_f2m_mul_mod(a: &[u64], b: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_reduce(bc_poly_mul_raw(a, b), m as usize, ks)
}

pub(crate) fn bc_f2m_square_mod(a: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_reduce(bc_poly_square_raw(a), m as usize, ks)
}

pub(crate) fn bc_f2m_square_plus_product(
    a: &[u64],
    x: &[u64],
    y: &[u64],
    m: i32,
    ks: &[usize],
) -> Vec<u64> {
    bc_poly_reduce(
        bc_poly_add(bc_poly_square_raw(a), &bc_poly_mul_raw(x, y)),
        m as usize,
        ks,
    )
}

pub(crate) fn bc_f2m_multiply_plus_product(
    a: &[u64],
    b: &[u64],
    x: &[u64],
    y: &[u64],
    m: i32,
    ks: &[usize],
) -> Vec<u64> {
    bc_poly_reduce(
        bc_poly_add(bc_poly_mul_raw(a, b), &bc_poly_mul_raw(x, y)),
        m as usize,
        ks,
    )
}

pub(crate) fn bc_f2m_inverse_mod(
    a: &[u64],
    m: i32,
    ks: &[usize],
) -> Result<Vec<u64>, MethodCallFailed> {
    bc_poly_inverse(a, m as usize, ks).ok_or_else(|| {
        RuntimeError::ArithmeticException {
            message: "LongArray: polynomial not invertible".into(),
        }
        .into()
    })
}

pub(crate) fn bc_f2m_div_mod(
    a: &[u64],
    b: &[u64],
    m: i32,
    ks: &[usize],
) -> Result<Vec<u64>, MethodCallFailed> {
    let inv = bc_f2m_inverse_mod(b, m, ks)?;
    Ok(bc_f2m_mul_mod(a, &inv, m, ks))
}

pub(crate) fn bc_f2m_sqrt_mod(a: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_mod_square_n(a.to_vec(), m.saturating_sub(1), m as usize, ks)
}

pub(crate) fn bc_f2m_add_one(mut a: Vec<u64>) -> Vec<u64> {
    bc_poly_flip_bit(&mut a, 0);
    bc_trim_poly(&mut a);
    a
}

pub(crate) fn bc_f2m_curve_coord(ctx: &dyn NativeContext, curve: ObjectRef) -> i32 {
    match ctx.get_field_by_name(curve, "coord") {
        Value::Int(coord) => coord,
        _ => 0,
    }
}

pub(crate) fn bc_f2m_curve_field_words(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
    name: &str,
) -> Result<(i32, ObjectRef, Vec<usize>, Vec<u64>), MethodCallFailed> {
    let field = bc_fp_obj_field(ctx, curve, name)?;
    bc_f2m_read_this(ctx, field)
}

pub(crate) fn bc_f2m_point_read(
    ctx: &dyn NativeContext,
    point: ObjectRef,
) -> Result<Option<BcF2mPointData>, MethodCallFailed> {
    let curve = match ctx.get_field_by_name(point, "curve") {
        Value::Object(Some(curve)) => curve,
        _ => return Err(bc_f2m_bad_state("ECPoint.F2m: point has no curve")),
    };
    let coord = bc_f2m_curve_coord(ctx, curve);
    if !matches!(coord, 0 | 1 | 6) {
        return Err(bc_f2m_bad_state(
            "ECPoint.F2m: unsupported coordinate system",
        ));
    }
    let x_obj = match ctx.get_field_by_name(point, "x") {
        Value::Object(Some(x)) => x,
        _ => return Ok(None),
    };
    let y_obj = match ctx.get_field_by_name(point, "y") {
        Value::Object(Some(y)) => y,
        _ => return Ok(None),
    };
    let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, x_obj)?;
    let (_, _, _, y) = bc_f2m_read_this(ctx, y_obj)?;
    let z_obj = match ctx.get_field_by_name(point, "zs") {
        Value::Object(Some(zs)) if ctx.array_length(zs) > 0 => match ctx.get_array_element(zs, 0) {
            Value::Object(Some(z)) => Some(z),
            _ => None,
        },
        _ => None,
    };
    let z = match z_obj {
        Some(z) => bc_f2m_read_this(ctx, z)?.3,
        None => vec![1],
    };

    Ok(Some(BcF2mPointData {
        curve,
        coord,
        m,
        ks_obj,
        ks,
        x,
        y,
        z,
    }))
}

pub(crate) fn bc_f2m_point_infinity(ctx: &dyn NativeContext, curve: ObjectRef) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_f2m_bad_state("ECPoint.F2m: curve has no infinity point")),
    }
}

pub(crate) fn bc_f2m_alloc_point(
    ctx: &mut dyn NativeContext,
    p: &BcF2mPointData,
    x: &[u64],
    y: &[u64],
    z: &[u64],
) -> Result<ObjectRef, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(p.curve);
    let ks_pin = ctx.pin_native_root(p.ks_obj);
    let ks_obj = ctx.read_native_pin(ks_pin, p.ks_obj);
    let x_obj = bc_f2m_alloc(ctx, p.m, ks_obj, x)?;
    let x_pin = ctx.pin_native_root(x_obj);
    let ks_obj = ctx.read_native_pin(ks_pin, p.ks_obj);
    let y_obj = bc_f2m_alloc(ctx, p.m, ks_obj, y)?;
    let y_pin = ctx.pin_native_root(y_obj);
    let z_pair = if p.coord == 0 {
        None
    } else {
        let ks_obj = ctx.read_native_pin(ks_pin, p.ks_obj);
        let z_obj = bc_f2m_alloc(ctx, p.m, ks_obj, z)?;
        Some((z_obj, ctx.pin_native_root(z_obj)))
    };

    let result = (|| {
        let z_len = if p.coord == 0 { 0 } else { 1 };
        let zs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, z_len);
        let zs_pin = ctx.pin_native_root(zs);
        if let Some((z_obj, z_pin)) = z_pair.as_ref() {
            ctx.set_array_element(
                zs,
                0,
                Value::Object(Some(ctx.read_native_pin(*z_pin, *z_obj))),
            );
        }

        match ctx.new_object("org/bouncycastle/math/ec/ECPoint$F2m")? {
            Some(Value::Object(Some(point))) => {
                ctx.set_field_by_name(
                    point,
                    "curve",
                    Value::Object(Some(ctx.read_native_pin(base_pin, p.curve))),
                );
                ctx.set_field_by_name(
                    point,
                    "x",
                    Value::Object(Some(ctx.read_native_pin(x_pin, x_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "y",
                    Value::Object(Some(ctx.read_native_pin(y_pin, y_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "zs",
                    Value::Object(Some(ctx.read_native_pin(zs_pin, zs))),
                );
                Ok(point)
            }
            _ => Err(bc_f2m_bad_state("ECPoint.F2m: allocation failed")),
        }
    })();

    ctx.unpin_native_roots(base_pin);
    result
}

pub(crate) fn bc_f2m_point_affine_double_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let l1 = bc_f2m_add2(&bc_f2m_div_mod(&p.y, &p.x, p.m, &p.ks)?, &p.x);
    let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let x3 = bc_f2m_add_many(&bc_f2m_square_mod(&l1, p.m, &p.ks), &[&l1, &curve_a]);
    let y3 = bc_f2m_square_plus_product(&p.x, &x3, &bc_f2m_add_one(l1.clone()), p.m, &p.ks);
    Ok(Some((x3, y3, vec![1])))
}

pub(crate) fn bc_f2m_point_homogeneous_double_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let z1_is_one = bc_poly_is_one(&p.z);
    let x1z1 = if z1_is_one {
        p.x.clone()
    } else {
        bc_f2m_mul_mod(&p.x, &p.z, p.m, &p.ks)
    };
    let y1z1 = if z1_is_one {
        p.y.clone()
    } else {
        bc_f2m_mul_mod(&p.y, &p.z, p.m, &p.ks)
    };
    let x1_sq = bc_f2m_square_mod(&p.x, p.m, &p.ks);
    let s = bc_f2m_add2(&x1_sq, &y1z1);
    let v = x1z1;
    let v_squared = bc_f2m_square_mod(&v, p.m, &p.ks);
    let sv = bc_f2m_add2(&s, &v);
    let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let h = bc_f2m_multiply_plus_product(&sv, &s, &v_squared, &curve_a, p.m, &p.ks);
    let x3 = bc_f2m_mul_mod(&v, &h, p.m, &p.ks);
    let y3 = bc_f2m_multiply_plus_product(
        &bc_f2m_square_mod(&x1_sq, p.m, &p.ks),
        &v,
        &h,
        &sv,
        p.m,
        &p.ks,
    );
    let z3 = bc_f2m_mul_mod(&v, &v_squared, p.m, &p.ks);
    Ok(Some((x3, y3, z3)))
}

pub(crate) fn bc_f2m_point_lambda_double_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let z_is_one = bc_poly_is_one(&p.z);
    let l1z1 = if z_is_one {
        p.y.clone()
    } else {
        bc_f2m_mul_mod(&p.y, &p.z, p.m, &p.ks)
    };
    let z1_sq = if z_is_one {
        p.z.clone()
    } else {
        bc_f2m_square_mod(&p.z, p.m, &p.ks)
    };
    let (_, _, _, a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let a_z1_sq = if z_is_one {
        a.clone()
    } else {
        bc_f2m_mul_mod(&a, &z1_sq, p.m, &p.ks)
    };
    let t = bc_f2m_add_many(&bc_f2m_square_mod(&p.y, p.m, &p.ks), &[&l1z1, &a_z1_sq]);

    let (_, _, _, b) = bc_f2m_curve_field_words(ctx, p.curve, "b")?;
    if bc_poly_is_zero(&t) {
        return Ok(Some((t, bc_f2m_sqrt_mod(&b, p.m, &p.ks), vec![1])));
    }

    let x3 = bc_f2m_square_mod(&t, p.m, &p.ks);
    let z3 = if z_is_one {
        t.clone()
    } else {
        bc_f2m_mul_mod(&t, &z1_sq, p.m, &p.ks)
    };

    let l3 = if bc_poly_degree(&b) < ((p.m as usize) >> 1) {
        let t1 = bc_f2m_square_mod(&bc_f2m_add2(&p.y, &p.x), p.m, &p.ks);
        let t2 = if bc_poly_is_one(&b) {
            bc_f2m_square_mod(&bc_f2m_add2(&a_z1_sq, &z1_sq), p.m, &p.ks)
        } else {
            bc_f2m_square_plus_product(
                &a_z1_sq,
                &b,
                &bc_f2m_square_mod(&z1_sq, p.m, &p.ks),
                p.m,
                &p.ks,
            )
        };
        let mut l3 = bc_f2m_add_many(
            &bc_f2m_mul_mod(&bc_f2m_add_many(&t1, &[&t, &z1_sq]), &t1, p.m, &p.ks),
            &[&t2, &x3],
        );
        if bc_poly_is_zero(&a) {
            l3 = bc_f2m_add2(&l3, &z3);
        } else if !bc_poly_is_one(&a) {
            l3 = bc_f2m_add2(
                &l3,
                &bc_f2m_mul_mod(&bc_f2m_add_one(a.clone()), &z3, p.m, &p.ks),
            );
        }
        l3
    } else {
        let x1z1 = if z_is_one {
            p.x.clone()
        } else {
            bc_f2m_mul_mod(&p.x, &p.z, p.m, &p.ks)
        };
        bc_f2m_add_many(
            &bc_f2m_square_plus_product(&x1z1, &t, &l1z1, p.m, &p.ks),
            &[&x3, &z3],
        )
    };

    Ok(Some((x3, l3, z3)))
}

pub(crate) fn bc_f2m_point_double_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    if bc_poly_is_zero(&p.x) {
        return Ok(None);
    }

    match p.coord {
        0 => bc_f2m_point_affine_double_data(ctx, p),
        1 => bc_f2m_point_homogeneous_double_data(ctx, p),
        6 => bc_f2m_point_lambda_double_data(ctx, p),
        _ => Err(bc_f2m_bad_state(
            "ECPoint.F2m: unsupported coordinate system",
        )),
    }
}

pub(crate) fn bc_f2m_point_affine_add_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
    q: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let dx = bc_f2m_add2(&p.x, &q.x);
    let dy = bc_f2m_add2(&p.y, &q.y);
    if bc_poly_is_zero(&dx) {
        return if bc_poly_is_zero(&dy) {
            bc_f2m_point_double_data(ctx, p)
        } else {
            Ok(None)
        };
    }
    let l = bc_f2m_div_mod(&dy, &dx, p.m, &p.ks)?;
    let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let x3 = bc_f2m_add_many(&bc_f2m_square_mod(&l, p.m, &p.ks), &[&l, &dx, &curve_a]);
    let y3 = bc_f2m_add_many(
        &bc_f2m_mul_mod(&l, &bc_f2m_add2(&p.x, &x3), p.m, &p.ks),
        &[&x3, &p.y],
    );
    Ok(Some((x3, y3, vec![1])))
}

pub(crate) fn bc_f2m_point_homogeneous_add_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
    q: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let z2_is_one = bc_poly_is_one(&q.z);
    let u1 = bc_f2m_mul_mod(&p.z, &q.y, p.m, &p.ks);
    let u2 = if z2_is_one {
        p.y.clone()
    } else {
        bc_f2m_mul_mod(&p.y, &q.z, p.m, &p.ks)
    };
    let u = bc_f2m_add2(&u1, &u2);
    let v1 = bc_f2m_mul_mod(&p.z, &q.x, p.m, &p.ks);
    let v2 = if z2_is_one {
        p.x.clone()
    } else {
        bc_f2m_mul_mod(&p.x, &q.z, p.m, &p.ks)
    };
    let v = bc_f2m_add2(&v1, &v2);

    if bc_poly_is_zero(&v) {
        return if bc_poly_is_zero(&u) {
            bc_f2m_point_double_data(ctx, p)
        } else {
            Ok(None)
        };
    }

    let v_sq = bc_f2m_square_mod(&v, p.m, &p.ks);
    let v_cu = bc_f2m_mul_mod(&v_sq, &v, p.m, &p.ks);
    let w = if z2_is_one {
        p.z.clone()
    } else {
        bc_f2m_mul_mod(&p.z, &q.z, p.m, &p.ks)
    };
    let uv = bc_f2m_add2(&u, &v);
    let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let a0 = bc_f2m_multiply_plus_product(&uv, &u, &v_sq, &curve_a, p.m, &p.ks);
    let a = bc_f2m_add2(&bc_f2m_mul_mod(&a0, &w, p.m, &p.ks), &v_cu);
    let x3 = bc_f2m_mul_mod(&v, &a, p.m, &p.ks);
    let v_sq_z2 = if z2_is_one {
        v_sq
    } else {
        bc_f2m_mul_mod(&v_sq, &q.z, p.m, &p.ks)
    };
    let y0 = bc_f2m_multiply_plus_product(&u, &p.x, &v, &p.y, p.m, &p.ks);
    let y3 = bc_f2m_multiply_plus_product(&y0, &v_sq_z2, &uv, &a, p.m, &p.ks);
    let z3 = bc_f2m_mul_mod(&v_cu, &w, p.m, &p.ks);
    Ok(Some((x3, y3, z3)))
}

pub(crate) fn bc_f2m_point_lambda_add_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
    q: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    if bc_poly_is_zero(&p.x) {
        if bc_poly_is_zero(&q.x) {
            return Ok(None);
        }
        return bc_f2m_point_add_data(ctx, q, p);
    }

    let z1_is_one = bc_poly_is_one(&p.z);
    let (u2, s2) = if z1_is_one {
        (q.x.clone(), q.y.clone())
    } else {
        (
            bc_f2m_mul_mod(&q.x, &p.z, p.m, &p.ks),
            bc_f2m_mul_mod(&q.y, &p.z, p.m, &p.ks),
        )
    };
    let z2_is_one = bc_poly_is_one(&q.z);
    let (u1, s1) = if z2_is_one {
        (p.x.clone(), p.y.clone())
    } else {
        (
            bc_f2m_mul_mod(&p.x, &q.z, p.m, &p.ks),
            bc_f2m_mul_mod(&p.y, &q.z, p.m, &p.ks),
        )
    };

    let a = bc_f2m_add2(&s1, &s2);
    let mut b = bc_f2m_add2(&u1, &u2);
    if bc_poly_is_zero(&b) {
        return if bc_poly_is_zero(&a) {
            bc_f2m_point_double_data(ctx, p)
        } else {
            Ok(None)
        };
    }

    if bc_poly_is_zero(&q.x) {
        let z_inv = if z1_is_one {
            vec![1]
        } else {
            bc_f2m_inverse_mod(&p.z, p.m, &p.ks)?
        };
        let x1 = if z1_is_one {
            p.x.clone()
        } else {
            bc_f2m_mul_mod(&p.x, &z_inv, p.m, &p.ks)
        };
        let raw_y1 = bc_f2m_mul_mod(&bc_f2m_add2(&p.y, &p.x), &p.x, p.m, &p.ks);
        let y1 = if z1_is_one {
            raw_y1
        } else {
            bc_f2m_mul_mod(&raw_y1, &bc_f2m_square_mod(&z_inv, p.m, &p.ks), p.m, &p.ks)
        };
        let l = bc_f2m_div_mod(&bc_f2m_add2(&y1, &q.y), &x1, p.m, &p.ks)?;
        let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
        let x3 = bc_f2m_add_many(&bc_f2m_square_mod(&l, p.m, &p.ks), &[&l, &x1, &curve_a]);
        if bc_poly_is_zero(&x3) {
            let (_, _, _, curve_b) = bc_f2m_curve_field_words(ctx, p.curve, "b")?;
            return Ok(Some((x3, bc_f2m_sqrt_mod(&curve_b, p.m, &p.ks), vec![1])));
        }
        let y3 = bc_f2m_add_many(
            &bc_f2m_mul_mod(&l, &bc_f2m_add2(&x1, &x3), p.m, &p.ks),
            &[&x3, &y1],
        );
        let l3 = bc_f2m_add2(&bc_f2m_div_mod(&y3, &x3, p.m, &p.ks)?, &x3);
        return Ok(Some((x3, l3, vec![1])));
    }

    b = bc_f2m_square_mod(&b, p.m, &p.ks);
    let au1 = bc_f2m_mul_mod(&a, &u1, p.m, &p.ks);
    let au2 = bc_f2m_mul_mod(&a, &u2, p.m, &p.ks);
    let x3 = bc_f2m_mul_mod(&au1, &au2, p.m, &p.ks);
    if bc_poly_is_zero(&x3) {
        let (_, _, _, curve_b) = bc_f2m_curve_field_words(ctx, p.curve, "b")?;
        return Ok(Some((x3, bc_f2m_sqrt_mod(&curve_b, p.m, &p.ks), vec![1])));
    }

    let mut abz2 = bc_f2m_mul_mod(&a, &b, p.m, &p.ks);
    if !z2_is_one {
        abz2 = bc_f2m_mul_mod(&abz2, &q.z, p.m, &p.ks);
    }
    let l3 = bc_f2m_square_plus_product(
        &bc_f2m_add2(&au2, &b),
        &abz2,
        &bc_f2m_add2(&p.y, &p.z),
        p.m,
        &p.ks,
    );
    let z3 = if z1_is_one {
        abz2
    } else {
        bc_f2m_mul_mod(&abz2, &p.z, p.m, &p.ks)
    };

    Ok(Some((x3, l3, z3)))
}

pub(crate) fn bc_f2m_point_add_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
    q: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    match p.coord {
        0 => bc_f2m_point_affine_add_data(ctx, p, q),
        1 => bc_f2m_point_homogeneous_add_data(ctx, p, q),
        6 => bc_f2m_point_lambda_add_data(ctx, p, q),
        _ => Err(bc_f2m_bad_state(
            "ECPoint.F2m: unsupported coordinate system",
        )),
    }
}

pub(crate) fn bc_f2m_point_return_twice(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(p) = bc_f2m_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    match bc_f2m_point_double_data(ctx, &p)? {
        Some((x, y, z)) => Ok(Some(Value::Object(Some(bc_f2m_alloc_point(
            ctx, &p, &x, &y, &z,
        )?)))),
        None => bc_f2m_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_f2m_point_return_add(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let Some(p) = bc_f2m_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let Some(q) = bc_f2m_point_read(ctx, other)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    match bc_f2m_point_add_data(ctx, &p, &q)? {
        Some((x, y, z)) => Ok(Some(Value::Object(Some(bc_f2m_alloc_point(
            ctx, &p, &x, &y, &z,
        )?)))),
        None => bc_f2m_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_f2m_point_return_twice_plus(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let Some(p) = bc_f2m_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let Some(q) = bc_f2m_point_read(ctx, other)? else {
        return bc_f2m_point_return_twice(ctx, &[Value::Object(Some(this))]);
    };
    let Some((dx, dy, dz)) = bc_f2m_point_double_data(ctx, &p)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let doubled = BcF2mPointData {
        curve: p.curve,
        coord: p.coord,
        m: p.m,
        ks_obj: p.ks_obj,
        ks: p.ks.clone(),
        x: dx,
        y: dy,
        z: dz,
    };
    match bc_f2m_point_add_data(ctx, &doubled, &q)? {
        Some((x, y, z)) => Ok(Some(Value::Object(Some(bc_f2m_alloc_point(
            ctx, &p, &x, &y, &z,
        )?)))),
        None => bc_f2m_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn register_bc_f2m_point(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/math/ec/ECPoint$F2m";
    r.register(
        cls,
        "add",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_add,
    );
    r.register(
        cls,
        "twice",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_twice,
    );
    r.register(
        cls,
        "twicePlus",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_twice_plus,
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i8> {
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as i8,
            _ => 0,
        });
    }
    out
}

pub(crate) fn bc_ec_point_virtual_obj(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.invoke_virtual(receiver, method_name, descriptor, args)? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: format!("ECAlgorithms: {method_name} returned null"),
        }
        .into()),
    }
}

pub(crate) fn bc_ecalg_impl_shamirs_trick_jsf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let p_orig = obj_arg(args, 0)?;
    let k_orig = obj_arg(args, 1)?;
    let q_orig = obj_arg(args, 2)?;
    let l_orig = obj_arg(args, 3)?;

    let base_pin = ctx.pin_native_root(p_orig);
    let k_pin = ctx.pin_native_root(k_orig);
    let q_pin = ctx.pin_native_root(q_orig);
    let l_pin = ctx.pin_native_root(l_orig);

    let k_live = ctx.read_native_pin(k_pin, k_orig);
    let l_live = ctx.read_native_pin(l_pin, l_orig);
    let jsf = match ctx.invoke(
        "org/bouncycastle/math/ec/WNafUtil",
        "generateJSF",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)[B",
        &[Value::Object(Some(k_live)), Value::Object(Some(l_live))],
    )? {
        Some(Value::Object(Some(arr))) => arr,
        _ => {
            ctx.unpin_native_roots(base_pin);
            return Err(RuntimeError::IllegalStateException {
                message: "ECAlgorithms: generateJSF returned null".into(),
            }
            .into());
        }
    };
    let jsf_pin = ctx.pin_native_root(jsf);

    let p_cur = ctx.read_native_pin(base_pin, p_orig);
    let curve = match ctx.get_field_by_name(p_cur, "curve") {
        Value::Object(Some(curve)) => curve,
        _ => {
            ctx.unpin_native_roots(base_pin);
            return Err(bc_f2m_bad_state("ECAlgorithms: point has no curve"));
        }
    };
    let curve_pin = ctx.pin_native_root(curve);
    let infinity = match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => infinity,
        _ => {
            ctx.unpin_native_roots(base_pin);
            return Err(bc_f2m_bad_state("ECAlgorithms: curve has no infinity"));
        }
    };
    let infinity_pin = ctx.pin_native_root(infinity);

    let p_live = ctx.read_native_pin(base_pin, p_orig);
    let q_live = ctx.read_native_pin(q_pin, q_orig);
    let p_add_q = bc_ec_point_virtual_obj(
        ctx,
        p_live,
        "add",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        &[Value::Object(Some(q_live))],
    )?;
    let p_add_q_pin = ctx.pin_native_root(p_add_q);

    let p_live = ctx.read_native_pin(base_pin, p_orig);
    let q_live = ctx.read_native_pin(q_pin, q_orig);
    let p_sub_q = bc_ec_point_virtual_obj(
        ctx,
        p_live,
        "subtract",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        &[Value::Object(Some(q_live))],
    )?;
    let p_sub_q_pin = ctx.pin_native_root(p_sub_q);

    let q_live = ctx.read_native_pin(q_pin, q_orig);
    let q_neg = bc_ec_point_virtual_obj(
        ctx,
        q_live,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        &[],
    )?;
    let q_neg_pin = ctx.pin_native_root(q_neg);
    let p_sub_q_live = ctx.read_native_pin(p_sub_q_pin, p_sub_q);
    let p_sub_q_neg = bc_ec_point_virtual_obj(
        ctx,
        p_sub_q_live,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        &[],
    )?;
    let p_sub_q_neg_pin = ctx.pin_native_root(p_sub_q_neg);
    let p_live = ctx.read_native_pin(base_pin, p_orig);
    let p_neg = bc_ec_point_virtual_obj(
        ctx,
        p_live,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        &[],
    )?;
    let p_neg_pin = ctx.pin_native_root(p_neg);
    let p_add_q_live = ctx.read_native_pin(p_add_q_pin, p_add_q);
    let p_add_q_neg = bc_ec_point_virtual_obj(
        ctx,
        p_add_q_live,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        &[],
    )?;
    let p_add_q_neg_pin = ctx.pin_native_root(p_add_q_neg);

    let jsf_bytes = bc_read_byte_array(ctx, ctx.read_native_pin(jsf_pin, jsf));
    let mut r = ctx.read_native_pin(infinity_pin, infinity);
    let mut r_pin = ctx.pin_native_root(r);

    for &jsfi in jsf_bytes.iter().rev() {
        let jsfi_i32 = jsfi as i32;
        let k_digit = (jsfi_i32 << 24) >> 28;
        let l_digit = (jsfi_i32 << 28) >> 28;
        let index = 4 + (k_digit * 3) + l_digit;
        let table_obj = match index {
            0 => ctx.read_native_pin(p_add_q_neg_pin, p_add_q_neg),
            1 => ctx.read_native_pin(p_neg_pin, p_neg),
            2 => ctx.read_native_pin(p_sub_q_neg_pin, p_sub_q_neg),
            3 => ctx.read_native_pin(q_neg_pin, q_neg),
            4 => ctx.read_native_pin(infinity_pin, infinity),
            5 => ctx.read_native_pin(q_pin, q_orig),
            6 => ctx.read_native_pin(p_sub_q_pin, p_sub_q),
            7 => ctx.read_native_pin(base_pin, p_orig),
            8 => ctx.read_native_pin(p_add_q_pin, p_add_q),
            _ => {
                ctx.unpin_native_roots(base_pin);
                return Err(RuntimeError::IllegalStateException {
                    message: "ECAlgorithms: invalid JSF table index".into(),
                }
                .into());
            }
        };
        let r_live = ctx.read_native_pin(r_pin, r);
        let next = bc_ec_point_virtual_obj(
            ctx,
            r_live,
            "twicePlus",
            "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
            &[Value::Object(Some(table_obj))],
        )?;
        let next_pin = ctx.pin_native_root(next);
        r = ctx.read_native_pin(next_pin, next);
        r_pin = next_pin;
    }

    let result = ctx.read_native_pin(r_pin, r);
    let _ = ctx.read_native_pin(curve_pin, curve);
    ctx.unpin_native_roots(base_pin);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn register_bc_ec_algorithms(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        "org/bouncycastle/math/ec/ECAlgorithms",
        "implShamirsTrickJsf",
        "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_ecalg_impl_shamirs_trick_jsf,
    );

    r.set_category(__prev_cat);
}

#[derive(Clone, Copy)]
pub(crate) struct BcSecTFieldSpec {
    m: usize,
    ks: &'static [usize],
}

impl BcSecTFieldSpec {
    fn words(self) -> usize {
        (self.m + 63) >> 6
    }

    fn ext_words(self) -> usize {
        (self.m * 2 + 63) >> 6
    }
}

pub(crate) fn bc_sect_aioobe(index: usize) -> MethodCallFailed {
    RuntimeError::aioobe_index_only(index.min(i32::MAX as usize) as i32).into()
}

pub(crate) fn bc_read_long_array_fixed(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    len: usize,
) -> Result<Vec<u64>, MethodCallFailed> {
    if ctx.array_length(arr) < len {
        return Err(bc_sect_aioobe(len));
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(match ctx.get_array_element(arr, i) {
            Value::Long(v) => v as u64,
            Value::Int(v) => v as u32 as u64,
            _ => 0,
        });
    }
    Ok(out)
}

pub(crate) fn bc_write_long_array_fixed(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    values: &[u64],
    len: usize,
) -> Result<(), MethodCallFailed> {
    if ctx.array_length(arr) < len {
        return Err(bc_sect_aioobe(len));
    }
    for i in 0..len {
        ctx.set_array_element(
            arr,
            i,
            Value::Long(values.get(i).copied().unwrap_or(0) as i64),
        );
    }
    Ok(())
}

pub(crate) fn bc_xor_into_long_array_fixed(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    values: &[u64],
    len: usize,
) -> Result<(), MethodCallFailed> {
    if ctx.array_length(arr) < len {
        return Err(bc_sect_aioobe(len));
    }
    for i in 0..len {
        let cur = match ctx.get_array_element(arr, i) {
            Value::Long(v) => v as u64,
            Value::Int(v) => v as u32 as u64,
            _ => 0,
        };
        ctx.set_array_element(
            arr,
            i,
            Value::Long((cur ^ values.get(i).copied().unwrap_or(0)) as i64),
        );
    }
    Ok(())
}

pub(crate) fn bc_alloc_jlong_array(
    ctx: &mut dyn NativeContext,
    values: &[u64],
    len: usize,
) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, len);
    for i in 0..len {
        ctx.set_array_element(
            arr,
            i,
            Value::Long(values.get(i).copied().unwrap_or(0) as i64),
        );
    }
    arr
}

pub(crate) fn bc_sect_half_trace(x: Vec<u64>, spec: BcSecTFieldSpec) -> Vec<u64> {
    let mut z = x.clone();
    for _ in (1..spec.m).step_by(2) {
        z = bc_poly_mod_square_n(z, 2, spec.m, spec.ks);
        z = bc_poly_add(z, &x);
    }
    z
}

pub(crate) fn bc_sect_trace(x: Vec<u64>, spec: BcSecTFieldSpec) -> i32 {
    let mut t = bc_poly_reduce(x, spec.m, spec.ks);
    let mut acc = t.clone();
    for _ in 1..spec.m {
        t = bc_poly_mod_square_n(t, 1, spec.m, spec.ks);
        acc = bc_poly_add(acc, &t);
    }
    (acc.first().copied().unwrap_or(0) & 1) as i32
}

pub(crate) fn bc_sect_add_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let words = spec.words();
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, words)?;
    let y = bc_read_long_array_fixed(ctx, y_arr, words)?;
    bc_write_long_array_fixed(ctx, z_arr, &bc_poly_add(x, &y), words)?;
    Ok(None)
}

pub(crate) fn bc_sect_add_both_to_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let words = spec.words();
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, words)?;
    let y = bc_read_long_array_fixed(ctx, y_arr, words)?;
    bc_xor_into_long_array_fixed(ctx, z_arr, &bc_poly_add(x, &y), words)?;
    Ok(None)
}

pub(crate) fn bc_sect_add_ext_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let ext_words = spec.ext_words();
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, ext_words)?;
    let y = bc_read_long_array_fixed(ctx, y_arr, ext_words)?;
    bc_write_long_array_fixed(ctx, z_arr, &bc_poly_add(x, &y), ext_words)?;
    Ok(None)
}

pub(crate) fn bc_sect_add_one_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let words = spec.words();
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let mut x = bc_read_long_array_fixed(ctx, x_arr, words)?;
    bc_poly_flip_bit(&mut x, 0);
    bc_trim_poly(&mut x);
    bc_write_long_array_fixed(ctx, z_arr, &x, words)?;
    Ok(None)
}

pub(crate) fn bc_sect_multiply_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let words = spec.words();
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, words)?;
    let y = bc_read_long_array_fixed(ctx, y_arr, words)?;
    let z = bc_poly_reduce(bc_poly_mul_raw(&x, &y), spec.m, spec.ks);
    bc_write_long_array_fixed(ctx, z_arr, &z, words)?;
    Ok(None)
}

pub(crate) fn bc_sect_multiply_add_to_ext_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let words = spec.words();
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let zz_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, words)?;
    let y = bc_read_long_array_fixed(ctx, y_arr, words)?;
    bc_xor_into_long_array_fixed(ctx, zz_arr, &bc_poly_mul_raw(&x, &y), spec.ext_words())?;
    Ok(None)
}

pub(crate) fn bc_sect_reduce_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let xx_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let xx = bc_read_long_array_fixed(ctx, xx_arr, spec.ext_words())?;
    bc_write_long_array_fixed(
        ctx,
        z_arr,
        &bc_poly_reduce(xx, spec.m, spec.ks),
        spec.words(),
    )?;
    Ok(None)
}

pub(crate) fn bc_sect_sqrt_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let z = bc_poly_mod_square_n(x, spec.m.saturating_sub(1) as i32, spec.m, spec.ks);
    bc_write_long_array_fixed(ctx, z_arr, &z, spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_square_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let z = bc_poly_reduce(bc_poly_square_raw(&x), spec.m, spec.ks);
    bc_write_long_array_fixed(ctx, z_arr, &z, spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_square_add_to_ext_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let zz_arr = obj_arg(args, 1)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    bc_xor_into_long_array_fixed(ctx, zz_arr, &bc_poly_square_raw(&x), spec.ext_words())?;
    Ok(None)
}

pub(crate) fn bc_sect_square_n_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let z = bc_poly_mod_square_n(x, n, spec.m, spec.ks);
    bc_write_long_array_fixed(ctx, z_arr, &z, spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_invert_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let inv = bc_poly_inverse(&x, spec.m, spec.ks).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalStateException {
            message: "SecTField: zero is not invertible".into(),
        })
    })?;
    bc_write_long_array_fixed(ctx, z_arr, &inv, spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_half_trace_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    bc_write_long_array_fixed(ctx, z_arr, &bc_sect_half_trace(x, spec), spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_trace_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    Ok(Some(Value::Int(bc_sect_trace(x, spec))))
}

pub(crate) fn bc_sect_precomp_multiplicand_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    Ok(Some(Value::Object(Some(bc_alloc_jlong_array(
        ctx,
        &x,
        spec.words(),
    )))))
}

pub(crate) fn bc_sect_multiply_precomp_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let precomp_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let y = bc_read_long_array_fixed(ctx, precomp_arr, spec.words())?;
    let z = bc_poly_reduce(bc_poly_mul_raw(&x, &y), spec.m, spec.ks);
    bc_write_long_array_fixed(ctx, z_arr, &z, spec.words())?;
    Ok(None)
}

pub(crate) fn bc_sect_multiply_precomp_add_to_ext_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let precomp_arr = obj_arg(args, 1)?;
    let zz_arr = obj_arg(args, 2)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    let y = bc_read_long_array_fixed(ctx, precomp_arr, spec.words())?;
    bc_xor_into_long_array_fixed(ctx, zz_arr, &bc_poly_mul_raw(&x, &y), spec.ext_words())?;
    Ok(None)
}

macro_rules! define_bc_sect_module {
    ($module:ident, $m:expr, [$($ks:expr),+ $(,)?]) => {
        mod $module {
            use super::*;
            const KS: &[usize] = &[$($ks),+];
            const SPEC: BcSecTFieldSpec = BcSecTFieldSpec { m: $m, ks: KS };

            pub(super) fn add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_native(ctx, args, SPEC)
            }
            pub(super) fn add_both_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_both_to_native(ctx, args, SPEC)
            }
            pub(super) fn add_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_ext_native(ctx, args, SPEC)
            }
            pub(super) fn add_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_add_one_native(ctx, args, SPEC)
            }
            pub(super) fn multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_add_to_ext_native(ctx, args, SPEC)
            }
            pub(super) fn reduce(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_reduce_native(ctx, args, SPEC)
            }
            pub(super) fn sqrt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_sqrt_native(ctx, args, SPEC)
            }
            pub(super) fn square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_native(ctx, args, SPEC)
            }
            pub(super) fn square_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_add_to_ext_native(ctx, args, SPEC)
            }
            pub(super) fn square_n(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_square_n_native(ctx, args, SPEC)
            }
            pub(super) fn invert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_invert_native(ctx, args, SPEC)
            }
            pub(super) fn half_trace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_half_trace_native(ctx, args, SPEC)
            }
            pub(super) fn trace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_trace_native(ctx, args, SPEC)
            }
            pub(super) fn precomp_multiplicand(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_precomp_multiplicand_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_precomp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_precomp_native(ctx, args, SPEC)
            }
            pub(super) fn multiply_precomp_add_to_ext(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                bc_sect_multiply_precomp_add_to_ext_native(ctx, args, SPEC)
            }
        }
    };
}

define_bc_sect_module!(bc_sect113, 113, [9]);

define_bc_sect_module!(bc_sect131, 131, [2, 3, 8]);

define_bc_sect_module!(bc_sect163, 163, [3, 6, 7]);

define_bc_sect_module!(bc_sect193, 193, [15]);

define_bc_sect_module!(bc_sect233, 233, [74]);

define_bc_sect_module!(bc_sect239, 239, [158]);

define_bc_sect_module!(bc_sect283, 283, [5, 7, 12]);

define_bc_sect_module!(bc_sect409, 409, [87]);

define_bc_sect_module!(bc_sect571, 571, [2, 5, 10]);

macro_rules! register_bc_sect_module {
    ($r:expr, $class:literal, $module:ident) => {{
        $r.register($class, "add", "([J[J[J)V", $module::add);
        $r.register($class, "addBothTo", "([J[J[J)V", $module::add_both_to);
        $r.register($class, "addExt", "([J[J[J)V", $module::add_ext);
        $r.register($class, "addOne", "([J[J)V", $module::add_one);
        $r.register($class, "multiply", "([J[J[J)V", $module::multiply);
        $r.register(
            $class,
            "multiplyAddToExt",
            "([J[J[J)V",
            $module::multiply_add_to_ext,
        );
        $r.register($class, "reduce", "([J[J)V", $module::reduce);
        $r.register($class, "sqrt", "([J[J)V", $module::sqrt);
        $r.register($class, "square", "([J[J)V", $module::square);
        $r.register(
            $class,
            "squareAddToExt",
            "([J[J)V",
            $module::square_add_to_ext,
        );
        $r.register($class, "squareN", "([JI[J)V", $module::square_n);
        $r.register($class, "invert", "([J[J)V", $module::invert);
        $r.register($class, "halfTrace", "([J[J)V", $module::half_trace);
        $r.register($class, "trace", "([J)I", $module::trace);
    }};
}

pub(crate) fn register_bc_sect571_precomp(r: &mut NativeMethodRegistry) {
    let cls = "org/bouncycastle/math/ec/custom/sec/SecT571Field";
    r.register(
        cls,
        "precompMultiplicand",
        "([J)[J",
        bc_sect571::precomp_multiplicand,
    );
    r.register(
        cls,
        "multiplyPrecomp",
        "([J[J[J)V",
        bc_sect571::multiply_precomp,
    );
    r.register(
        cls,
        "multiplyPrecompAddToExt",
        "([J[J[J)V",
        bc_sect571::multiply_precomp_add_to_ext,
    );
}

pub(crate) fn register_bc_sect_field_kernels(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT113Field",
        bc_sect113
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT131Field",
        bc_sect131
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT163Field",
        bc_sect163
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT193Field",
        bc_sect193
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT233Field",
        bc_sect233
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT239Field",
        bc_sect239
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT283Field",
        bc_sect283
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT409Field",
        bc_sect409
    );
    register_bc_sect_module!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571Field",
        bc_sect571
    );
    register_bc_sect571_precomp(r);
    r.set_category(__prev_cat);
}

#[derive(Clone, Copy)]
pub(crate) enum BcSecTPointFormula {
    General,
    KoblitzZeroA,
    SecT163K1,
}

#[derive(Clone, Copy)]
pub(crate) struct BcSecTPointSpec {
    point_class: &'static str,
    field_class: &'static str,
    field: BcSecTFieldSpec,
    formula: BcSecTPointFormula,
}

pub(crate) struct BcSecTPointData {
    curve: ObjectRef,
    x: Vec<u64>,
    y: Vec<u64>,
    z: Vec<u64>,
    spec: BcSecTPointSpec,
}

pub(crate) fn bc_poly_is_zero(x: &[u64]) -> bool {
    x.iter().all(|&w| w == 0)
}

pub(crate) fn bc_poly_is_one(x: &[u64]) -> bool {
    x.first().copied().unwrap_or(0) == 1 && x.iter().skip(1).all(|&w| w == 0)
}

pub(crate) fn bc_sect_add2(a: &[u64], b: &[u64]) -> Vec<u64> {
    bc_poly_add(a.to_vec(), b)
}

pub(crate) fn bc_sect_add_many(first: &[u64], rest: &[&[u64]]) -> Vec<u64> {
    let mut out = first.to_vec();
    for part in rest {
        out = bc_poly_add(out, part);
    }
    out
}

pub(crate) fn bc_sect_mul(a: &[u64], b: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_reduce(bc_poly_mul_raw(a, b), spec.m, spec.ks)
}

pub(crate) fn bc_sect_square(a: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_reduce(bc_poly_square_raw(a), spec.m, spec.ks)
}

pub(crate) fn bc_sect_sqrt(a: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_mod_square_n(a.to_vec(), spec.m.saturating_sub(1) as i32, spec.m, spec.ks)
}

pub(crate) fn bc_sect_square_plus_product(
    a: &[u64],
    x: &[u64],
    y: &[u64],
    spec: BcSecTFieldSpec,
) -> Vec<u64> {
    bc_poly_reduce(
        bc_poly_add(bc_poly_square_raw(a), &bc_poly_mul_raw(x, y)),
        spec.m,
        spec.ks,
    )
}

pub(crate) fn bc_sect_field_words(
    ctx: &dyn NativeContext,
    field_obj: ObjectRef,
    spec: BcSecTFieldSpec,
) -> Result<Vec<u64>, MethodCallFailed> {
    let arr = bc_fp_obj_field(ctx, field_obj, "x")?;
    bc_read_long_array_fixed(ctx, arr, spec.words())
}

pub(crate) fn bc_sect_curve_field_words(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
    name: &str,
    spec: BcSecTFieldSpec,
) -> Result<Vec<u64>, MethodCallFailed> {
    let field = bc_fp_obj_field(ctx, curve, name)?;
    bc_sect_field_words(ctx, field, spec)
}

pub(crate) fn bc_sect_point_infinity(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_fp_bad_state("SecTPoint: curve has no infinity point")),
    }
}

pub(crate) fn bc_sect_point_read(
    ctx: &dyn NativeContext,
    point: ObjectRef,
    spec: BcSecTPointSpec,
) -> Result<Option<BcSecTPointData>, MethodCallFailed> {
    let curve = match ctx.get_field_by_name(point, "curve") {
        Value::Object(Some(curve)) => curve,
        _ => return Err(bc_fp_bad_state("SecTPoint: point has no curve")),
    };
    let x_obj = match ctx.get_field_by_name(point, "x") {
        Value::Object(Some(x)) => x,
        _ => return Ok(None),
    };
    let y_obj = match ctx.get_field_by_name(point, "y") {
        Value::Object(Some(y)) => y,
        _ => return Ok(None),
    };
    let z_obj = match ctx.get_field_by_name(point, "zs") {
        Value::Object(Some(zs)) if ctx.array_length(zs) > 0 => match ctx.get_array_element(zs, 0) {
            Value::Object(Some(z)) => Some(z),
            _ => None,
        },
        _ => None,
    };

    let x = bc_sect_field_words(ctx, x_obj, spec.field)?;
    let y = bc_sect_field_words(ctx, y_obj, spec.field)?;
    let z = match z_obj {
        Some(z) => bc_sect_field_words(ctx, z, spec.field)?,
        None => vec![1],
    };

    Ok(Some(BcSecTPointData {
        curve,
        x,
        y,
        z,
        spec,
    }))
}

pub(crate) fn bc_sect_alloc_field_element(
    ctx: &mut dyn NativeContext,
    spec: BcSecTPointSpec,
    words: &[u64],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = bc_alloc_jlong_array(ctx, words, spec.field.words());
    let arr_pin = ctx.pin_native_root(arr);
    let result = match ctx.new_object(spec.field_class)? {
        Some(Value::Object(Some(obj))) => {
            ctx.set_field_by_name(
                obj,
                "x",
                Value::Object(Some(ctx.read_native_pin(arr_pin, arr))),
            );
            Ok(obj)
        }
        _ => Err(bc_fp_bad_state("SecTFieldElement: allocation failed")),
    };
    ctx.unpin_native_roots(arr_pin);
    result
}

pub(crate) fn bc_sect_alloc_point(
    ctx: &mut dyn NativeContext,
    spec: BcSecTPointSpec,
    curve: ObjectRef,
    x: &[u64],
    y: &[u64],
    z: &[u64],
) -> Result<ObjectRef, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(curve);
    let x_obj = bc_sect_alloc_field_element(ctx, spec, x)?;
    let x_pin = ctx.pin_native_root(x_obj);
    let y_obj = bc_sect_alloc_field_element(ctx, spec, y)?;
    let y_pin = ctx.pin_native_root(y_obj);
    let z_obj = bc_sect_alloc_field_element(ctx, spec, z)?;
    let z_pin = ctx.pin_native_root(z_obj);

    let result = (|| {
        let zs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        let zs_pin = ctx.pin_native_root(zs);
        ctx.set_array_element(
            zs,
            0,
            Value::Object(Some(ctx.read_native_pin(z_pin, z_obj))),
        );

        match ctx.new_object(spec.point_class)? {
            Some(Value::Object(Some(point))) => {
                ctx.set_field_by_name(
                    point,
                    "curve",
                    Value::Object(Some(ctx.read_native_pin(base_pin, curve))),
                );
                ctx.set_field_by_name(
                    point,
                    "x",
                    Value::Object(Some(ctx.read_native_pin(x_pin, x_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "y",
                    Value::Object(Some(ctx.read_native_pin(y_pin, y_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "zs",
                    Value::Object(Some(ctx.read_native_pin(zs_pin, zs))),
                );
                Ok(point)
            }
            _ => Err(bc_fp_bad_state("SecTPoint: allocation failed")),
        }
    })();

    ctx.unpin_native_roots(base_pin);
    result
}

pub(crate) fn bc_sect_point_double_data(
    ctx: &dyn NativeContext,
    p: &BcSecTPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    if bc_poly_is_zero(&p.x) {
        return Ok(None);
    }

    let spec = p.spec.field;
    let z_is_one = bc_poly_is_one(&p.z);
    let l1z1 = if z_is_one {
        p.y.clone()
    } else {
        bc_sect_mul(&p.y, &p.z, spec)
    };
    let z1_sq = if z_is_one {
        p.z.clone()
    } else {
        bc_sect_square(&p.z, spec)
    };

    let t = match p.spec.formula {
        BcSecTPointFormula::General => {
            let a = bc_sect_curve_field_words(ctx, p.curve, "a", spec)?;
            let a_z1_sq = if z_is_one {
                a
            } else {
                bc_sect_mul(&a, &z1_sq, spec)
            };
            bc_sect_add_many(&bc_sect_square(&p.y, spec), &[&l1z1, &a_z1_sq])
        }
        BcSecTPointFormula::KoblitzZeroA => {
            if z_is_one {
                bc_sect_add2(&bc_sect_square(&p.y, spec), &p.y)
            } else {
                bc_sect_mul(&bc_sect_add2(&p.y, &p.z), &p.y, spec)
            }
        }
        BcSecTPointFormula::SecT163K1 => {
            bc_sect_add_many(&bc_sect_square(&p.y, spec), &[&l1z1, &z1_sq])
        }
    };

    if bc_poly_is_zero(&t) {
        let b = bc_sect_curve_field_words(ctx, p.curve, "b", spec)?;
        let y = match p.spec.formula {
            BcSecTPointFormula::General => bc_sect_sqrt(&b, spec),
            _ => b,
        };
        return Ok(Some((t, y, vec![1])));
    }

    let x3 = bc_sect_square(&t, spec);
    let z3 = if z_is_one {
        t.clone()
    } else {
        bc_sect_mul(&t, &z1_sq, spec)
    };

    let l3 = match p.spec.formula {
        BcSecTPointFormula::General => {
            let x1z1 = if z_is_one {
                p.x.clone()
            } else {
                bc_sect_mul(&p.x, &p.z, spec)
            };
            bc_sect_add_many(
                &bc_sect_square_plus_product(&x1z1, &t, &l1z1, spec),
                &[&x3, &z3],
            )
        }
        BcSecTPointFormula::KoblitzZeroA => {
            let t1 = bc_sect_square(&bc_sect_add2(&p.y, &p.x), spec);
            let t2 = if z_is_one {
                p.z.clone()
            } else {
                bc_sect_square(&z1_sq, spec)
            };
            let factor = bc_sect_add_many(&t1, &[&t, &z1_sq]);
            bc_sect_add_many(&bc_sect_mul(&factor, &t1, spec), &[&t2, &x3, &z3])
        }
        BcSecTPointFormula::SecT163K1 => {
            let t1 = bc_sect_square(&bc_sect_add2(&p.y, &p.x), spec);
            let factor = bc_sect_add_many(&t1, &[&t, &z1_sq]);
            bc_sect_add2(&bc_sect_mul(&factor, &t1, spec), &x3)
        }
    };

    Ok(Some((x3, l3, z3)))
}

pub(crate) fn bc_sect_point_return_twice_with_spec(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTPointSpec,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(p) = bc_sect_point_read(ctx, this, spec)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    match bc_sect_point_double_data(ctx, &p)? {
        Some((x, y, z)) => Ok(Some(Value::Object(Some(bc_sect_alloc_point(
            ctx, spec, p.curve, &x, &y, &z,
        )?)))),
        None => bc_sect_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_sect_point_spec_for_class(class_name: &str) -> Option<BcSecTPointSpec> {
    macro_rules! spec {
        ($point:literal, $field:literal, $m:expr, [$($ks:expr),+ $(,)?], $formula:expr) => {
            BcSecTPointSpec {
                point_class: $point,
                field_class: $field,
                field: BcSecTFieldSpec {
                    m: $m,
                    ks: &[$($ks),+],
                },
                formula: $formula,
            }
        };
    }

    Some(match class_name {
        "org/bouncycastle/math/ec/custom/sec/SecT113R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT113R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT113FieldElement",
            113,
            [9],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT113R2Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT113R2Point",
            "org/bouncycastle/math/ec/custom/sec/SecT113FieldElement",
            113,
            [9],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT131R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT131R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT131FieldElement",
            131,
            [2, 3, 8],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT131R2Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT131R2Point",
            "org/bouncycastle/math/ec/custom/sec/SecT131FieldElement",
            131,
            [2, 3, 8],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT163K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT163K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT163FieldElement",
            163,
            [3, 6, 7],
            BcSecTPointFormula::SecT163K1
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT163R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT163R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT163FieldElement",
            163,
            [3, 6, 7],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT163R2Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT163R2Point",
            "org/bouncycastle/math/ec/custom/sec/SecT163FieldElement",
            163,
            [3, 6, 7],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT193R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT193R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT193FieldElement",
            193,
            [15],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT193R2Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT193R2Point",
            "org/bouncycastle/math/ec/custom/sec/SecT193FieldElement",
            193,
            [15],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT233K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT233K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT233FieldElement",
            233,
            [74],
            BcSecTPointFormula::KoblitzZeroA
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT233R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT233R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT233FieldElement",
            233,
            [74],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT239K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT239K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT239FieldElement",
            239,
            [158],
            BcSecTPointFormula::KoblitzZeroA
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT283K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT283K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT283FieldElement",
            283,
            [5, 7, 12],
            BcSecTPointFormula::KoblitzZeroA
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT283R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT283R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT283FieldElement",
            283,
            [5, 7, 12],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT409K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT409K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT409FieldElement",
            409,
            [87],
            BcSecTPointFormula::KoblitzZeroA
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT409R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT409R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT409FieldElement",
            409,
            [87],
            BcSecTPointFormula::General
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT571K1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT571K1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT571FieldElement",
            571,
            [2, 5, 10],
            BcSecTPointFormula::KoblitzZeroA
        ),
        "org/bouncycastle/math/ec/custom/sec/SecT571R1Point" => spec!(
            "org/bouncycastle/math/ec/custom/sec/SecT571R1Point",
            "org/bouncycastle/math/ec/custom/sec/SecT571FieldElement",
            571,
            [2, 5, 10],
            BcSecTPointFormula::General
        ),
        _ => return None,
    })
}

pub(crate) fn bc_ec_point_return_times_pow2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let e = match args.get(1).copied() {
        Some(Value::Int(e)) => e,
        _ => return Err(bc_fp_bad_state("ECPoint.timesPow2: missing int exponent")),
    };
    if e < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "'e' cannot be negative".into(),
        }
        .into());
    }
    if e == 0 {
        return Ok(Some(Value::Object(Some(this))));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    if let Some(spec) = bc_sect_point_spec_for_class(&class_name) {
        let Some(mut p) = bc_sect_point_read(ctx, this, spec)? else {
            return Ok(Some(Value::Object(Some(this))));
        };
        for _ in 0..e {
            let Some((x, y, z)) = bc_sect_point_double_data(ctx, &p)? else {
                return bc_sect_point_infinity(ctx, p.curve);
            };
            p.x = x;
            p.y = y;
            p.z = z;
        }
        return Ok(Some(Value::Object(Some(bc_sect_alloc_point(
            ctx, spec, p.curve, &p.x, &p.y, &p.z,
        )?))));
    }

    let mut point = this;
    for _ in 0..e {
        match ctx.invoke_virtual(point, "twice", "()Lorg/bouncycastle/math/ec/ECPoint;", &[])? {
            Some(Value::Object(Some(next))) => point = next,
            _ => return Err(bc_fp_bad_state("ECPoint.timesPow2: twice returned null")),
        }
    }
    Ok(Some(Value::Object(Some(point))))
}

macro_rules! define_bc_sect_point_module {
    ($module:ident, $class:literal) => {
        mod $module {
            use super::*;

            pub(super) fn twice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
                let spec = bc_sect_point_spec_for_class($class)
                    .ok_or_else(|| bc_fp_bad_state("SecTPoint: missing point spec"))?;
                bc_sect_point_return_twice_with_spec(ctx, args, spec)
            }
        }
    };
}

define_bc_sect_point_module!(
    bc_sect113r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT113R1Point"
);

define_bc_sect_point_module!(
    bc_sect113r2_point,
    "org/bouncycastle/math/ec/custom/sec/SecT113R2Point"
);

define_bc_sect_point_module!(
    bc_sect131r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT131R1Point"
);

define_bc_sect_point_module!(
    bc_sect131r2_point,
    "org/bouncycastle/math/ec/custom/sec/SecT131R2Point"
);

define_bc_sect_point_module!(
    bc_sect163k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT163K1Point"
);

define_bc_sect_point_module!(
    bc_sect163r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT163R1Point"
);

define_bc_sect_point_module!(
    bc_sect163r2_point,
    "org/bouncycastle/math/ec/custom/sec/SecT163R2Point"
);

define_bc_sect_point_module!(
    bc_sect193r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT193R1Point"
);

define_bc_sect_point_module!(
    bc_sect193r2_point,
    "org/bouncycastle/math/ec/custom/sec/SecT193R2Point"
);

define_bc_sect_point_module!(
    bc_sect233k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT233K1Point"
);

define_bc_sect_point_module!(
    bc_sect233r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT233R1Point"
);

define_bc_sect_point_module!(
    bc_sect239k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT239K1Point"
);

define_bc_sect_point_module!(
    bc_sect283k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT283K1Point"
);

define_bc_sect_point_module!(
    bc_sect283r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT283R1Point"
);

define_bc_sect_point_module!(
    bc_sect409k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT409K1Point"
);

define_bc_sect_point_module!(
    bc_sect409r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT409R1Point"
);

define_bc_sect_point_module!(
    bc_sect571k1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT571K1Point"
);

define_bc_sect_point_module!(
    bc_sect571r1_point,
    "org/bouncycastle/math/ec/custom/sec/SecT571R1Point"
);

macro_rules! register_bc_sect_point {
    ($r:expr, $class:literal, $module:ident) => {
        $r.register(
            $class,
            "twice",
            "()Lorg/bouncycastle/math/ec/ECPoint;",
            $module::twice,
        );
    };
}

pub(crate) fn register_bc_sect_point_methods(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/math/ec/ECPoint",
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_ec_point_return_times_pow2,
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT113R1Point",
        bc_sect113r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT113R2Point",
        bc_sect113r2_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT131R1Point",
        bc_sect131r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT131R2Point",
        bc_sect131r2_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT163K1Point",
        bc_sect163k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT163R1Point",
        bc_sect163r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT163R2Point",
        bc_sect163r2_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT193R1Point",
        bc_sect193r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT193R2Point",
        bc_sect193r2_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT233K1Point",
        bc_sect233k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT233R1Point",
        bc_sect233r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT239K1Point",
        bc_sect239k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT283K1Point",
        bc_sect283k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT283R1Point",
        bc_sect283r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT409K1Point",
        bc_sect409k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT409R1Point",
        bc_sect409r1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571K1Point",
        bc_sect571k1_point
    );
    register_bc_sect_point!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571R1Point",
        bc_sect571r1_point
    );
    r.set_category(__prev_cat);
}

#[cfg(test)]
pub(crate) mod bc_longarray_poly_tests {
    use super::{bc_poly_inverse, bc_poly_mul_raw, bc_poly_reduce};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn sect131r1_longarray_inverse_matches_bc() {
        let m = 131;
        let ks = [2usize, 3, 8];
        let x = vec![0x0f9c_1813_4363_8399, 0x81ba_f91f_df98_33c4, 0];
        let expected = vec![
            0x4bae_8672_4f5c_7253,
            0xcdf6_616b_d803_be96,
            0x0000_0000_0000_0003,
        ];

        let inv = bc_poly_inverse(&x, m, &ks).expect("sect131r1 x must be invertible");
        assert_eq!(expected, inv);
        assert_eq!(vec![1], bc_poly_reduce(bc_poly_mul_raw(&x, &inv), m, &ks));

        let x_trimmed = vec![0x0f9c_1813_4363_8399, 0x81ba_f91f_df98_33c4];
        let inv = bc_poly_inverse(&x_trimmed, m, &ks).expect("trimmed x must be invertible");
        assert_eq!(expected, inv);
        assert_eq!(
            vec![1],
            bc_poly_reduce(bc_poly_mul_raw(&x_trimmed, &inv), m, &ks)
        );
    }
}

pub(crate) fn bc_fp_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_fp_obj_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_fp_bad_state("ECFieldElement.Fp: malformed object field")),
    }
}

pub(crate) fn bc_fp_optional_obj_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Value {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Value::Object(Some(o)),
        _ => Value::Object(None),
    }
}

pub(crate) fn bc_fp_read_this(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(ObjectRef, Value, crate::bigint::BigInt), MethodCallFailed> {
    let q_obj = bc_fp_obj_field(ctx, this, "q")?;
    let r_val = bc_fp_optional_obj_field(ctx, this, "r");
    let x_obj = bc_fp_obj_field(ctx, this, "x")?;
    Ok((q_obj, r_val, bi_read_int(ctx, x_obj)))
}

pub(crate) fn bc_fp_arg_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let obj = obj_arg(args, idx)?;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(obj))
        .as_deref()
        == Some("org/bouncycastle/math/ec/ECFieldElement$Fp")
    {
        let x_obj = bc_fp_obj_field(ctx, obj, "x")?;
        return Ok(bi_read_int(ctx, x_obj));
    }

    let value = ctx.invoke_virtual(obj, "toBigInteger", "()Ljava/math/BigInteger;", &[])?;
    match value {
        Some(Value::Object(Some(o))) => Ok(bi_read_int(ctx, o)),
        _ => Err(bc_fp_bad_state(
            "ECFieldElement.Fp: toBigInteger returned null",
        )),
    }
}

pub(crate) fn bc_fp_mod_reduce(
    mut x: crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
    r: Option<&crate::bigint::BigInt>,
) -> crate::bigint::BigInt {
    if let Some(r) = r {
        let negative = x.is_neg();
        if negative {
            x = x.neg_value();
        }
        let q_len = q.bit_length();
        let one = bc_bigint_small(1);
        let r_is_one = r.cmp(&one) == std::cmp::Ordering::Equal;
        while x.bit_length() > q_len + 1 {
            let mut u = x.shr(q_len);
            let v = x.sub(&u.shl(q_len));
            if !r_is_one {
                u = u.mul(r);
            }
            x = u.add(&v);
        }
        while x.cmp(q) != std::cmp::Ordering::Less {
            x = x.sub(q);
        }
        if negative && !x.is_zero() {
            x = q.sub(&x);
        }
        x
    } else {
        x.modulo(q)
    }
}

pub(crate) fn bc_fp_alloc(
    ctx: &mut dyn NativeContext,
    q_obj: ObjectRef,
    r_val: Value,
    x: &crate::bigint::BigInt,
) -> Result<ObjectRef, MethodCallFailed> {
    let q_pin = ctx.pin_native_root(q_obj);
    let r_pin = match r_val {
        Value::Object(Some(r_obj)) => Some((ctx.pin_native_root(r_obj), r_obj)),
        _ => None,
    };

    let x_obj = bi_alloc_int(ctx, x)?;
    let x_pin = ctx.pin_native_root(x_obj);
    let obj_result = ctx.new_object("org/bouncycastle/math/ec/ECFieldElement$Fp");
    let result = match obj_result {
        Ok(Some(Value::Object(Some(obj)))) => {
            let q_val = Value::Object(Some(ctx.read_native_pin(q_pin, q_obj)));
            let r_val = match r_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => Value::Object(None),
            };
            ctx.set_field_by_name(obj, "q", q_val);
            ctx.set_field_by_name(obj, "r", r_val);
            ctx.set_field_by_name(
                obj,
                "x",
                Value::Object(Some(ctx.read_native_pin(x_pin, x_obj))),
            );
            Ok(obj)
        }
        Ok(_) => Err(bc_fp_bad_state("ECFieldElement.Fp: allocation failed")),
        Err(e) => Err(e),
    };
    ctx.unpin_native_roots(q_pin);
    result
}

pub(crate) fn bc_fp_return(
    ctx: &mut dyn NativeContext,
    q_obj: ObjectRef,
    r_val: Value,
    x: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_fp_alloc(
        ctx, q_obj, r_val, &x,
    )?))))
}

pub(crate) fn bc_fp_return_bi(
    ctx: &mut dyn NativeContext,
    x: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &x)?))))
}

pub(crate) fn bc_fp_bigint_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    Ok(bi_read_int(ctx, obj_arg(args, idx)?))
}

pub(crate) fn bc_fp_q_r(
    ctx: &dyn NativeContext,
    q_obj: ObjectRef,
    r_val: Value,
) -> (crate::bigint::BigInt, Option<crate::bigint::BigInt>) {
    let q = bi_read_int(ctx, q_obj);
    let r = match r_val {
        Value::Object(Some(o)) => Some(bi_read_int(ctx, o)),
        _ => None,
    };
    (q, r)
}

#[derive(Clone)]
pub(crate) struct BcFpPoint {
    curve: ObjectRef,
    q_obj: ObjectRef,
    r_val: Value,
    q: crate::bigint::BigInt,
    a: crate::bigint::BigInt,
    x: crate::bigint::BigInt,
    y: crate::bigint::BigInt,
}

pub(crate) fn bc_fp_mod(
    x: crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    x.modulo(q)
}

pub(crate) fn bc_fp_add_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.add(b).modulo(q)
}

pub(crate) fn bc_fp_sub_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.sub(b).modulo(q)
}

pub(crate) fn bc_fp_mul_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.mul(b).modulo(q)
}

pub(crate) fn bc_fp_inverse_mod(
    a: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    a.mod_inverse(q).ok_or_else(|| {
        RuntimeError::ArithmeticException {
            message: "BigInteger not invertible".into(),
        }
        .into()
    })
}

pub(crate) fn bc_fp_field_x(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let x_obj = bc_fp_obj_field(ctx, obj, "x")?;
    Ok(bi_read_int(ctx, x_obj))
}

pub(crate) fn bc_fp_curve_coord(ctx: &dyn NativeContext, curve: ObjectRef) -> i32 {
    match ctx.get_field_by_name(curve, "coord") {
        Value::Int(coord) => coord,
        _ => 0,
    }
}

pub(crate) fn bc_fp_curve_a(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let a_obj = bc_fp_obj_field(ctx, curve, "a")?;
    bc_fp_field_x(ctx, a_obj)
}

pub(crate) fn bc_fp_point_infinity(ctx: &dyn NativeContext, curve: ObjectRef) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_fp_bad_state("ECPoint.Fp: curve has no infinity point")),
    }
}

pub(crate) fn bc_fp_point_read(
    ctx: &dyn NativeContext,
    point: ObjectRef,
) -> Result<Option<BcFpPoint>, MethodCallFailed> {
    let curve = match ctx.get_field_by_name(point, "curve") {
        Value::Object(Some(curve)) => curve,
        _ => return Err(bc_fp_bad_state("ECPoint.Fp: point has no curve")),
    };
    let x_obj = match ctx.get_field_by_name(point, "x") {
        Value::Object(Some(x)) => x,
        _ => return Ok(None),
    };
    let y_obj = match ctx.get_field_by_name(point, "y") {
        Value::Object(Some(y)) => y,
        _ => return Ok(None),
    };
    let (q_obj, r_val, mut x) = bc_fp_read_this(ctx, x_obj)?;
    let (_, _, mut y) = bc_fp_read_this(ctx, y_obj)?;
    let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
    let a = bc_fp_curve_a(ctx, curve)?;

    let z_obj = match ctx.get_field_by_name(point, "zs") {
        Value::Object(Some(zs)) if ctx.array_length(zs) > 0 => match ctx.get_array_element(zs, 0) {
            Value::Object(Some(z)) => Some(z),
            _ => None,
        },
        _ => None,
    };

    if let Some(z_obj) = z_obj {
        let z = bc_fp_field_x(ctx, z_obj)?;
        if z.is_zero() {
            return Ok(None);
        }
        let one = bc_bigint_small(1);
        if z.cmp(&one) != std::cmp::Ordering::Equal {
            let z_inv = bc_fp_inverse_mod(&z, &q)?;
            match bc_fp_curve_coord(ctx, curve) {
                1 => {
                    x = bc_fp_mul_mod(&x, &z_inv, &q);
                    y = bc_fp_mul_mod(&y, &z_inv, &q);
                }
                2 | 3 | 4 => {
                    let z_inv2 = bc_fp_mul_mod(&z_inv, &z_inv, &q);
                    let z_inv3 = bc_fp_mul_mod(&z_inv2, &z_inv, &q);
                    x = bc_fp_mul_mod(&x, &z_inv2, &q);
                    y = bc_fp_mul_mod(&y, &z_inv3, &q);
                }
                _ => {}
            }
        }
    }

    Ok(Some(BcFpPoint {
        curve,
        q_obj,
        r_val,
        q,
        a,
        x,
        y,
    }))
}

pub(crate) fn bc_fp_alloc_point(
    ctx: &mut dyn NativeContext,
    curve: ObjectRef,
    q_obj: ObjectRef,
    r_val: Value,
    q: &crate::bigint::BigInt,
    x: crate::bigint::BigInt,
    y: crate::bigint::BigInt,
) -> Result<ObjectRef, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(curve);
    let q_pin = ctx.pin_native_root(q_obj);
    let r_pin = match r_val {
        Value::Object(Some(r_obj)) => Some((ctx.pin_native_root(r_obj), r_obj)),
        _ => None,
    };

    let result = (|| {
        let q_now = ctx.read_native_pin(q_pin, q_obj);
        let r_now = match r_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => Value::Object(None),
        };
        let x_obj = bc_fp_alloc(ctx, q_now, r_now, &bc_fp_mod(x, q))?;
        let x_pin = ctx.pin_native_root(x_obj);

        let q_now = ctx.read_native_pin(q_pin, q_obj);
        let r_now = match r_pin {
            Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
            None => Value::Object(None),
        };
        let y_obj = bc_fp_alloc(ctx, q_now, r_now, &bc_fp_mod(y, q))?;
        let y_pin = ctx.pin_native_root(y_obj);

        let curve_now = ctx.read_native_pin(base_pin, curve);
        let coord = bc_fp_curve_coord(ctx, curve_now);
        let z_len = match coord {
            1 | 2 => 1,
            4 => 2,
            _ => 0,
        };
        let zs = ctx.new_array(cratonvm_types::ArrayElementType::Reference, z_len);
        let zs_pin = ctx.pin_native_root(zs);
        if z_len > 0 {
            let q_now = ctx.read_native_pin(q_pin, q_obj);
            let r_now = match r_pin {
                Some((pin, fallback)) => Value::Object(Some(ctx.read_native_pin(pin, fallback))),
                None => Value::Object(None),
            };
            let one_obj = bc_fp_alloc(ctx, q_now, r_now, &bc_bigint_small(1))?;
            let one_pin = ctx.pin_native_root(one_obj);
            let zs_now = ctx.read_native_pin(zs_pin, zs);
            ctx.set_array_element(
                zs_now,
                0,
                Value::Object(Some(ctx.read_native_pin(one_pin, one_obj))),
            );

            if z_len > 1 {
                let curve_now = ctx.read_native_pin(base_pin, curve);
                let a_obj = bc_fp_obj_field(ctx, curve_now, "a")?;
                let a_pin = ctx.pin_native_root(a_obj);
                let zs_now = ctx.read_native_pin(zs_pin, zs);
                ctx.set_array_element(
                    zs_now,
                    1,
                    Value::Object(Some(ctx.read_native_pin(a_pin, a_obj))),
                );
            }
        }

        let point_result = ctx.new_object("org/bouncycastle/math/ec/ECPoint$Fp");
        match point_result {
            Ok(Some(Value::Object(Some(point)))) => {
                ctx.set_field_by_name(
                    point,
                    "curve",
                    Value::Object(Some(ctx.read_native_pin(base_pin, curve))),
                );
                ctx.set_field_by_name(
                    point,
                    "x",
                    Value::Object(Some(ctx.read_native_pin(x_pin, x_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "y",
                    Value::Object(Some(ctx.read_native_pin(y_pin, y_obj))),
                );
                ctx.set_field_by_name(
                    point,
                    "zs",
                    Value::Object(Some(ctx.read_native_pin(zs_pin, zs))),
                );
                Ok(point)
            }
            Ok(_) => Err(bc_fp_bad_state("ECPoint.Fp: allocation failed")),
            Err(e) => Err(e),
        }
    })();

    ctx.unpin_native_roots(base_pin);
    result
}

pub(crate) fn bc_fp_point_from_xy(
    ctx: &mut dyn NativeContext,
    p: &BcFpPoint,
    x: crate::bigint::BigInt,
    y: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_fp_alloc_point(
        ctx, p.curve, p.q_obj, p.r_val, &p.q, x, y,
    )?))))
}

pub(crate) fn bc_fp_point_add_data(
    p: &BcFpPoint,
    q_point: &BcFpPoint,
) -> Result<Option<(crate::bigint::BigInt, crate::bigint::BigInt)>, MethodCallFailed> {
    let modulus = &p.q;
    let dx = bc_fp_sub_mod(&q_point.x, &p.x, modulus);
    let dy = bc_fp_sub_mod(&q_point.y, &p.y, modulus);
    if dx.is_zero() {
        return if dy.is_zero() {
            bc_fp_point_double_data(p)
        } else {
            Ok(None)
        };
    }
    let gamma = bc_fp_mul_mod(&dy, &bc_fp_inverse_mod(&dx, modulus)?, modulus);
    let x3 = bc_fp_sub_mod(
        &bc_fp_sub_mod(&bc_fp_mul_mod(&gamma, &gamma, modulus), &p.x, modulus),
        &q_point.x,
        modulus,
    );
    let y3 = bc_fp_sub_mod(
        &bc_fp_mul_mod(&gamma, &bc_fp_sub_mod(&p.x, &x3, modulus), modulus),
        &p.y,
        modulus,
    );
    Ok(Some((x3, y3)))
}

pub(crate) fn bc_fp_point_double_data(
    p: &BcFpPoint,
) -> Result<Option<(crate::bigint::BigInt, crate::bigint::BigInt)>, MethodCallFailed> {
    if p.y.is_zero() {
        return Ok(None);
    }
    let modulus = &p.q;
    let three = bc_bigint_small(3);
    let two = bc_bigint_small(2);
    let numerator = bc_fp_add_mod(
        &bc_fp_mul_mod(&three, &bc_fp_mul_mod(&p.x, &p.x, modulus), modulus),
        &p.a,
        modulus,
    );
    let denominator = bc_fp_mul_mod(&two, &p.y, modulus);
    let gamma = bc_fp_mul_mod(
        &numerator,
        &bc_fp_inverse_mod(&denominator, modulus)?,
        modulus,
    );
    let x3 = bc_fp_sub_mod(
        &bc_fp_mul_mod(&gamma, &gamma, modulus),
        &bc_fp_mul_mod(&two, &p.x, modulus),
        modulus,
    );
    let y3 = bc_fp_sub_mod(
        &bc_fp_mul_mod(&gamma, &bc_fp_sub_mod(&p.x, &x3, modulus), modulus),
        &p.y,
        modulus,
    );
    Ok(Some((x3, y3)))
}

pub(crate) fn bc_fp_point_negate_data(
    p: &BcFpPoint,
) -> (crate::bigint::BigInt, crate::bigint::BigInt) {
    let y = if p.y.is_zero() {
        crate::bigint::BigInt::zero()
    } else {
        p.q.sub(&p.y).modulo(&p.q)
    };
    (p.x.clone(), y)
}

pub(crate) fn bc_fp_point_return_add(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let Some(p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let Some(q_point) = bc_fp_point_read(ctx, other)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    match bc_fp_point_add_data(&p, &q_point)? {
        Some((x, y)) => bc_fp_point_from_xy(ctx, &p, x, y),
        None => bc_fp_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_fp_point_return_twice(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    match bc_fp_point_double_data(&p)? {
        Some((x, y)) => bc_fp_point_from_xy(ctx, &p, x, y),
        None => bc_fp_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_fp_point_return_negate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    let (x, y) = bc_fp_point_negate_data(&p);
    bc_fp_point_from_xy(ctx, &p, x, y)
}

pub(crate) fn bc_fp_point_return_twice_plus(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    if this == other {
        return bc_fp_point_return_three_times(ctx, args);
    }
    let Some(p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let Some(q_point) = bc_fp_point_read(ctx, other)? else {
        return bc_fp_point_return_twice(ctx, &[Value::Object(Some(this))]);
    };
    let Some((dx, dy)) = bc_fp_point_double_data(&p)? else {
        return Ok(Some(Value::Object(Some(other))));
    };
    let doubled = BcFpPoint {
        x: dx,
        y: dy,
        ..p.clone()
    };
    match bc_fp_point_add_data(&doubled, &q_point)? {
        Some((x, y)) => bc_fp_point_from_xy(ctx, &p, x, y),
        None => bc_fp_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_fp_point_return_three_times(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    let Some((dx, dy)) = bc_fp_point_double_data(&p)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    let doubled = BcFpPoint {
        x: dx,
        y: dy,
        ..p.clone()
    };
    match bc_fp_point_add_data(&doubled, &p)? {
        Some((x, y)) => bc_fp_point_from_xy(ctx, &p, x, y),
        None => bc_fp_point_infinity(ctx, p.curve),
    }
}

pub(crate) fn bc_fp_point_return_times_pow2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let e = match args.get(1).copied() {
        Some(Value::Int(e)) => e,
        _ => {
            return Err(bc_fp_bad_state(
                "ECPoint.Fp.timesPow2: missing int exponent",
            ))
        }
    };
    if e < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "'e' cannot be negative".into(),
        }
        .into());
    }
    if e == 0 {
        return Ok(Some(Value::Object(Some(this))));
    }
    let Some(mut p) = bc_fp_point_read(ctx, this)? else {
        return Ok(Some(Value::Object(Some(this))));
    };
    for _ in 0..e {
        let Some((x, y)) = bc_fp_point_double_data(&p)? else {
            return bc_fp_point_infinity(ctx, p.curve);
        };
        p.x = x;
        p.y = y;
    }
    bc_fp_point_from_xy(ctx, &p, p.x.clone(), p.y.clone())
}

pub(crate) fn register_bc_fp_point(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/math/ec/ECPoint$Fp";
    r.register(
        cls,
        "add",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_add,
    );
    r.register(
        cls,
        "twice",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_twice,
    );
    r.register(
        cls,
        "twicePlus",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_twice_plus,
    );
    r.register(
        cls,
        "threeTimes",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_three_times,
    );
    r.register(
        cls,
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_times_pow2,
    );
    r.register(
        cls,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_negate,
    );

    r.set_category(__prev_cat);
}

pub(crate) fn register_bc_fp_field_element(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/math/ec/ECFieldElement$Fp";

    r.register(
        cls,
        "add",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = x.add(&bc_fp_arg_value(ctx, args, 1)?);
            if z.cmp(&q) != std::cmp::Ordering::Less {
                z = z.sub(&q);
            }
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "addOne",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = x.add(&bc_bigint_small(1));
            if z.cmp(&q) == std::cmp::Ordering::Equal {
                z = crate::bigint::BigInt::zero();
            }
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "subtract",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = x.sub(&bc_fp_arg_value(ctx, args, 1)?);
            if z.signum() < 0 {
                z = z.add(&q);
            }
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "multiply",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let z = bc_fp_mod_reduce(x.mul(&bc_fp_arg_value(ctx, args, 1)?), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "square",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let z = bc_fp_mod_reduce(x.mul(&x), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            if x.is_zero() {
                return Ok(Some(Value::Object(Some(this))));
            }
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            bc_fp_return(ctx, q_obj, r_val, q.sub(&x))
        },
    );

    r.register(
        cls,
        "invert",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let inv = x.mod_inverse(&q).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "BigInteger not invertible.".into(),
                })
            })?;
            bc_fp_return(ctx, q_obj, r_val, inv)
        },
    );

    r.register(
        cls,
        "divide",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, x) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let rhs = bc_fp_arg_value(ctx, args, 1)?;
            let inv = rhs.mod_inverse(&q).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "BigInteger not invertible.".into(),
                })
            })?;
            let z = bc_fp_mod_reduce(x.mul(&inv), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "modAdd",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = bc_fp_bigint_arg(ctx, args, 1)?.add(&bc_fp_bigint_arg(ctx, args, 2)?);
            if z.cmp(&q) != std::cmp::Ordering::Less {
                z = z.sub(&q);
            }
            bc_fp_return_bi(ctx, z)
        },
    );

    r.register(
        cls,
        "modDouble",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = bc_fp_bigint_arg(ctx, args, 1)?.shl(1);
            if z.cmp(&q) != std::cmp::Ordering::Less {
                z = z.sub(&q);
            }
            bc_fp_return_bi(ctx, z)
        },
    );

    r.register(
        cls,
        "modHalf",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = bc_fp_bigint_arg(ctx, args, 1)?;
            if z.mag_le().first().is_some_and(|w| (w & 1) != 0) {
                z = z.add(&q);
            }
            bc_fp_return_bi(ctx, z.shr(1))
        },
    );

    r.register(
        cls,
        "modHalfAbs",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = bc_fp_bigint_arg(ctx, args, 1)?;
            if z.mag_le().first().is_some_and(|w| (w & 1) != 0) {
                z = q.sub(&z);
            }
            bc_fp_return_bi(ctx, z.shr(1))
        },
    );

    r.register(
        cls,
        "modInverse",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let x = bc_fp_bigint_arg(ctx, args, 1)?;
            let inv = x.mod_inverse(&q).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::ArithmeticException {
                    message: "BigInteger not invertible.".into(),
                })
            })?;
            bc_fp_return_bi(ctx, inv)
        },
    );

    r.register(
        cls,
        "modMult",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let z = bc_fp_mod_reduce(
                bc_fp_bigint_arg(ctx, args, 1)?.mul(&bc_fp_bigint_arg(ctx, args, 2)?),
                &q,
                r.as_ref(),
            );
            bc_fp_return_bi(ctx, z)
        },
    );

    r.register(
        cls,
        "modReduce",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let z = bc_fp_mod_reduce(bc_fp_bigint_arg(ctx, args, 1)?, &q, r.as_ref());
            bc_fp_return_bi(ctx, z)
        },
    );

    r.register(
        cls,
        "modSubtract",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, _) = bc_fp_read_this(ctx, this)?;
            let (q, _) = bc_fp_q_r(ctx, q_obj, r_val);
            let mut z = bc_fp_bigint_arg(ctx, args, 1)?.sub(&bc_fp_bigint_arg(ctx, args, 2)?);
            if z.signum() < 0 {
                z = z.add(&q);
            }
            bc_fp_return_bi(ctx, z)
        },
    );

    r.register(
        cls,
        "multiplyPlusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, a) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let b = bc_fp_arg_value(ctx, args, 1)?;
            let x = bc_fp_arg_value(ctx, args, 2)?;
            let y = bc_fp_arg_value(ctx, args, 3)?;
            let z = bc_fp_mod_reduce(a.mul(&b).add(&x.mul(&y)), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "multiplyMinusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, a) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let b = bc_fp_arg_value(ctx, args, 1)?;
            let x = bc_fp_arg_value(ctx, args, 2)?;
            let y = bc_fp_arg_value(ctx, args, 3)?;
            let z = bc_fp_mod_reduce(a.mul(&b).sub(&x.mul(&y)), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "squarePlusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, a) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let x = bc_fp_arg_value(ctx, args, 1)?;
            let y = bc_fp_arg_value(ctx, args, 2)?;
            let z = bc_fp_mod_reduce(a.mul(&a).add(&x.mul(&y)), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.register(
        cls,
        "squareMinusProduct",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (q_obj, r_val, a) = bc_fp_read_this(ctx, this)?;
            let (q, r) = bc_fp_q_r(ctx, q_obj, r_val);
            let x = bc_fp_arg_value(ctx, args, 1)?;
            let y = bc_fp_arg_value(ctx, args, 2)?;
            let z = bc_fp_mod_reduce(a.mul(&a).sub(&x.mul(&y)), &q, r.as_ref());
            bc_fp_return(ctx, q_obj, r_val, z)
        },
    );

    r.set_category(__prev_cat);
}

#[derive(Clone, Copy)]
pub(crate) enum BcDigestKind {
    Sha1,
    Sha256,
}

impl BcDigestKind {
    fn len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => crate::real_sha1(data),
            Self::Sha256 => crate::real_sha256(data),
        }
    }
}

pub(crate) fn bc_gost3411_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_gost3411_byte_field(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(digest, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_gost3411_bad_state("GOST3411Digest: malformed state")),
    }
}

pub(crate) fn bc_gost3411_read_32(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<[u8; 32], MethodCallFailed> {
    let arr = bc_gost3411_byte_field(ctx, digest, field)?;
    if ctx.array_length(arr) < 32 {
        return Err(bc_gost3411_bad_state("GOST3411Digest: short state array"));
    }
    let mut out = [0u8; 32];
    if ctx.read_byte_array_into(arr, 0, &mut out) != 32 {
        return Err(bc_gost3411_bad_state(
            "GOST3411Digest: failed to read state array",
        ));
    }
    Ok(out)
}

pub(crate) fn bc_gost3411_write_field(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    field: &str,
    bytes: &[u8; 32],
) -> Result<(), MethodCallFailed> {
    let arr = bc_gost3411_byte_field(ctx, digest, field)?;
    if ctx.array_length(arr) < bytes.len() || !ctx.write_byte_array_from(arr, 0, bytes) {
        return Err(bc_gost3411_bad_state(
            "GOST3411Digest: failed to write state array",
        ));
    }
    Ok(())
}

pub(crate) fn bc_gost3411_read_c(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[[u8; 32]; 4], MethodCallFailed> {
    let c_arr = bc_gost3411_byte_field(ctx, digest, "C")?;
    if ctx.array_length(c_arr) < 4 {
        return Err(bc_gost3411_bad_state("GOST3411Digest: short C array"));
    }
    let mut out = [[0u8; 32]; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        let row = match ctx.get_array_element(c_arr, i) {
            Value::Object(Some(o)) => o,
            _ => return Err(bc_gost3411_bad_state("GOST3411Digest: malformed C row")),
        };
        if ctx.array_length(row) < 32 || ctx.read_byte_array_into(row, 0, slot) != 32 {
            return Err(bc_gost3411_bad_state("GOST3411Digest: malformed C row"));
        }
    }
    Ok(out)
}

pub(crate) fn bc_gost3411_read_sbox(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[u8; 128], MethodCallFailed> {
    let sbox_arr = bc_gost3411_byte_field(ctx, digest, "sBox")?;
    if ctx.array_length(sbox_arr) != 128 {
        return Err(bc_gost3411_bad_state("GOST3411Digest: malformed S-box"));
    }
    let mut out = [0u8; 128];
    if ctx.read_byte_array_into(sbox_arr, 0, &mut out) != 128 {
        return Err(bc_gost3411_bad_state(
            "GOST3411Digest: failed to read S-box",
        ));
    }
    Ok(out)
}

pub(crate) fn bc_gost3411_a(block: &mut [u8; 32]) {
    let mut a = [0u8; 8];
    for j in 0..8 {
        a[j] = block[j] ^ block[j + 8];
    }
    block.copy_within(8..32, 0);
    block[24..32].copy_from_slice(&a);
}

pub(crate) fn bc_gost3411_p(input: &[u8; 32]) -> [u8; 32] {
    let mut key = [0u8; 32];
    for k in 0..8 {
        key[4 * k] = input[k];
        key[1 + 4 * k] = input[8 + k];
        key[2 + 4 * k] = input[16 + k];
        key[3 + 4 * k] = input[24 + k];
    }
    key
}

pub(crate) fn bc_gost3411_fw(block: &mut [u8; 32]) {
    let mut words = [0u16; 16];
    for (i, word) in words.iter_mut().enumerate() {
        let lo = block[i * 2] as u16;
        let hi = (block[i * 2 + 1] as u16) << 8;
        *word = lo | hi;
    }

    let mut shifted = [0u16; 16];
    shifted[..15].copy_from_slice(&words[1..]);
    shifted[15] = words[0] ^ words[1] ^ words[2] ^ words[3] ^ words[12] ^ words[15];

    for (i, word) in shifted.iter().enumerate() {
        block[i * 2] = *word as u8;
        block[i * 2 + 1] = (*word >> 8) as u8;
    }
}

pub(crate) fn bc_gost28147_key(key: &[u8; 32]) -> [u32; 8] {
    let mut out = [0u32; 8];
    for i in 0..8 {
        let o = i * 4;
        out[i] = u32::from_le_bytes([key[o], key[o + 1], key[o + 2], key[o + 3]]);
    }
    out
}

pub(crate) fn bc_gost28147_main_step(sbox: &[u8; 128], n1: u32, key: u32) -> u32 {
    let cm = key.wrapping_add(n1);
    let mut om = 0u32;
    for i in 0..8 {
        let nibble = ((cm >> (i * 4)) & 0x0f) as usize;
        om |= (sbox[i * 16 + nibble] as u32) << (i * 4);
    }
    om.rotate_left(11)
}

pub(crate) fn bc_gost28147_encrypt_block(
    sbox: &[u8; 128],
    key_bytes: &[u8; 32],
    input: &[u8],
) -> [u8; 8] {
    let key = bc_gost28147_key(key_bytes);
    let mut n1 = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    let mut n2 = u32::from_le_bytes([input[4], input[5], input[6], input[7]]);

    for _ in 0..3 {
        for &k in &key {
            let tmp = n1;
            n1 = n2 ^ bc_gost28147_main_step(sbox, n1, k);
            n2 = tmp;
        }
    }
    for &k in key[1..8].iter().rev() {
        let tmp = n1;
        n1 = n2 ^ bc_gost28147_main_step(sbox, n1, k);
        n2 = tmp;
    }
    n2 ^= bc_gost28147_main_step(sbox, n1, key[0]);

    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&n1.to_le_bytes());
    out[4..].copy_from_slice(&n2.to_le_bytes());
    out
}

pub(crate) fn bc_gost3411_process_block(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    input: ObjectRef,
    in_off: usize,
) -> MethodCallResult {
    if in_off.saturating_add(32) > ctx.array_length(input) {
        return Err(RuntimeError::aioobe_index_only(
            in_off.saturating_add(31).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    let mut h = bc_gost3411_read_32(ctx, digest, "H")?;
    let c = bc_gost3411_read_c(ctx, digest)?;
    let sbox = bc_gost3411_read_sbox(ctx, digest)?;

    let mut m = [0u8; 32];
    ctx.read_byte_array_into(input, in_off, &mut m);

    let mut s = [0u8; 32];
    let mut u = h;
    let mut v = m;
    let mut w = [0u8; 32];

    for j in 0..32 {
        w[j] = u[j] ^ v[j];
    }
    let key = bc_gost3411_p(&w);
    s[0..8].copy_from_slice(&bc_gost28147_encrypt_block(&sbox, &key, &h[0..8]));

    for i in 1..4 {
        bc_gost3411_a(&mut u);
        for j in 0..32 {
            u[j] ^= c[i][j];
        }
        bc_gost3411_a(&mut v);
        bc_gost3411_a(&mut v);
        for j in 0..32 {
            w[j] = u[j] ^ v[j];
        }
        let key = bc_gost3411_p(&w);
        let off = i * 8;
        s[off..off + 8].copy_from_slice(&bc_gost28147_encrypt_block(&sbox, &key, &h[off..off + 8]));
    }

    for _ in 0..12 {
        bc_gost3411_fw(&mut s);
    }
    for n in 0..32 {
        s[n] ^= m[n];
    }
    bc_gost3411_fw(&mut s);
    for n in 0..32 {
        s[n] ^= h[n];
    }
    for _ in 0..61 {
        bc_gost3411_fw(&mut s);
    }
    h = s;

    bc_gost3411_write_field(ctx, digest, "M", &m)?;
    bc_gost3411_write_field(ctx, digest, "S", &s)?;
    bc_gost3411_write_field(ctx, digest, "U", &u)?;
    bc_gost3411_write_field(ctx, digest, "V", &v)?;
    bc_gost3411_write_field(ctx, digest, "W", &w)?;
    bc_gost3411_write_field(ctx, digest, "H", &h)?;
    Ok(None)
}

/// Native fast-path for the GOST R 34.11-94 compression block used by
/// BouncyCastle's legacy `GOST3411Digest`. The crypto regression suite runs the
/// million-'a' digest under the standing `org/bouncycastle/*` JIT ban; the Java
/// block function repeatedly reinitializes `GOST28147Engine` and spends more
/// than a minute in interpreted byte loops. This keeps the ban intact while
/// folding just the pure compression block.
pub(crate) fn register_bc_gost3411_digest(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        "org/bouncycastle/crypto/digests/GOST3411Digest",
        "processBlock",
        "([BI)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let input = obj_arg(args, 1)?;
            let in_off = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
                _ => 0,
            };
            bc_gost3411_process_block(ctx, this, input, in_off)
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) const BC_WHIRLPOOL_SBOX: [u8; 256] = [
    0x18, 0x23, 0xc6, 0xe8, 0x87, 0xb8, 0x01, 0x4f, 0x36, 0xa6, 0xd2, 0xf5, 0x79, 0x6f, 0x91, 0x52,
    0x60, 0xbc, 0x9b, 0x8e, 0xa3, 0x0c, 0x7b, 0x35, 0x1d, 0xe0, 0xd7, 0xc2, 0x2e, 0x4b, 0xfe, 0x57,
    0x15, 0x77, 0x37, 0xe5, 0x9f, 0xf0, 0x4a, 0xda, 0x58, 0xc9, 0x29, 0x0a, 0xb1, 0xa0, 0x6b, 0x85,
    0xbd, 0x5d, 0x10, 0xf4, 0xcb, 0x3e, 0x05, 0x67, 0xe4, 0x27, 0x41, 0x8b, 0xa7, 0x7d, 0x95, 0xd8,
    0xfb, 0xee, 0x7c, 0x66, 0xdd, 0x17, 0x47, 0x9e, 0xca, 0x2d, 0xbf, 0x07, 0xad, 0x5a, 0x83, 0x33,
    0x63, 0x02, 0xaa, 0x71, 0xc8, 0x19, 0x49, 0xd9, 0xf2, 0xe3, 0x5b, 0x88, 0x9a, 0x26, 0x32, 0xb0,
    0xe9, 0x0f, 0xd5, 0x80, 0xbe, 0xcd, 0x34, 0x48, 0xff, 0x7a, 0x90, 0x5f, 0x20, 0x68, 0x1a, 0xae,
    0xb4, 0x54, 0x93, 0x22, 0x64, 0xf1, 0x73, 0x12, 0x40, 0x08, 0xc3, 0xec, 0xdb, 0xa1, 0x8d, 0x3d,
    0x97, 0x00, 0xcf, 0x2b, 0x76, 0x82, 0xd6, 0x1b, 0xb5, 0xaf, 0x6a, 0x50, 0x45, 0xf3, 0x30, 0xef,
    0x3f, 0x55, 0xa2, 0xea, 0x65, 0xba, 0x2f, 0xc0, 0xde, 0x1c, 0xfd, 0x4d, 0x92, 0x75, 0x06, 0x8a,
    0xb2, 0xe6, 0x0e, 0x1f, 0x62, 0xd4, 0xa8, 0x96, 0xf9, 0xc5, 0x25, 0x59, 0x84, 0x72, 0x39, 0x4c,
    0x5e, 0x78, 0x38, 0x8c, 0xd1, 0xa5, 0xe2, 0x61, 0xb3, 0x21, 0x9c, 0x1e, 0x43, 0xc7, 0xfc, 0x04,
    0x51, 0x99, 0x6d, 0x0d, 0xfa, 0xdf, 0x7e, 0x24, 0x3b, 0xab, 0xce, 0x11, 0x8f, 0x4e, 0xb7, 0xeb,
    0x3c, 0x81, 0x94, 0xf7, 0xb9, 0x13, 0x2c, 0xd3, 0xe7, 0x6e, 0xc4, 0x03, 0x56, 0x44, 0x7f, 0xa9,
    0x2a, 0xbb, 0xc1, 0x53, 0xdc, 0x0b, 0x9d, 0x6c, 0x31, 0x74, 0xf6, 0x46, 0xac, 0x89, 0x14, 0xe1,
    0x16, 0x3a, 0x69, 0x09, 0x70, 0xb6, 0xd0, 0xed, 0xcc, 0x42, 0x98, 0xa4, 0x28, 0x5c, 0xf8, 0x86,
];

pub(crate) struct BcWhirlpoolTables {
    c: [[u64; 256]; 8],
    rc: [u64; 11],
}

pub(crate) fn bc_whirlpool_tables() -> &'static BcWhirlpoolTables {
    static TABLES: std::sync::OnceLock<BcWhirlpoolTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let mut c = [[0u64; 256]; 8];
        for (i, &v1) in BC_WHIRLPOOL_SBOX.iter().enumerate() {
            let v1 = v1 as u32;
            let v2 = bc_whirlpool_mul_x(v1);
            let v4 = bc_whirlpool_mul_x(v2);
            let v5 = v4 ^ v1;
            let v8 = bc_whirlpool_mul_x(v4);
            let v9 = v8 ^ v1;

            c[0][i] = bc_whirlpool_pack(v1, v1, v4, v1, v8, v5, v2, v9);
            c[1][i] = bc_whirlpool_pack(v9, v1, v1, v4, v1, v8, v5, v2);
            c[2][i] = bc_whirlpool_pack(v2, v9, v1, v1, v4, v1, v8, v5);
            c[3][i] = bc_whirlpool_pack(v5, v2, v9, v1, v1, v4, v1, v8);
            c[4][i] = bc_whirlpool_pack(v8, v5, v2, v9, v1, v1, v4, v1);
            c[5][i] = bc_whirlpool_pack(v1, v8, v5, v2, v9, v1, v1, v4);
            c[6][i] = bc_whirlpool_pack(v4, v1, v8, v5, v2, v9, v1, v1);
            c[7][i] = bc_whirlpool_pack(v1, v4, v1, v8, v5, v2, v9, v1);
        }

        let mut rc = [0u64; 11];
        for r in 1..=10 {
            let i = 8 * (r - 1);
            rc[r] = (c[0][i] & 0xff00_0000_0000_0000)
                ^ (c[1][i + 1] & 0x00ff_0000_0000_0000)
                ^ (c[2][i + 2] & 0x0000_ff00_0000_0000)
                ^ (c[3][i + 3] & 0x0000_00ff_0000_0000)
                ^ (c[4][i + 4] & 0x0000_0000_ff00_0000)
                ^ (c[5][i + 5] & 0x0000_0000_00ff_0000)
                ^ (c[6][i + 6] & 0x0000_0000_0000_ff00)
                ^ (c[7][i + 7] & 0x0000_0000_0000_00ff);
        }

        BcWhirlpoolTables { c, rc }
    })
}

pub(crate) fn bc_whirlpool_mul_x(input: u32) -> u32 {
    (input << 1) ^ (0u32.wrapping_sub(input >> 7) & 0x011d)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bc_whirlpool_pack(
    b7: u32,
    b6: u32,
    b5: u32,
    b4: u32,
    b3: u32,
    b2: u32,
    b1: u32,
    b0: u32,
) -> u64 {
    ((b7 as u64) << 56)
        ^ ((b6 as u64) << 48)
        ^ ((b5 as u64) << 40)
        ^ ((b4 as u64) << 32)
        ^ ((b3 as u64) << 24)
        ^ ((b2 as u64) << 16)
        ^ ((b1 as u64) << 8)
        ^ b0 as u64
}

pub(crate) fn bc_whirlpool_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_whirlpool_array_field(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(digest, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_whirlpool_bad_state("WhirlpoolDigest: malformed state")),
    }
}

pub(crate) fn bc_whirlpool_read_long8(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<[u64; 8], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, field)?;
    if ctx.array_length(arr) < 8 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short long array"));
    }
    let mut out = [0u64; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_whirlpool_bad_state("WhirlpoolDigest: bad long array")),
        }
    }
    Ok(out)
}

pub(crate) fn bc_whirlpool_write_long8(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    field: &str,
    values: &[u64; 8],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, field)?;
    if ctx.array_length(arr) < 8 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short long array"));
    }
    for (i, &value) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(value as i64));
    }
    Ok(())
}

pub(crate) fn bc_whirlpool_read_buffer(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[u8; 64], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_buffer")?;
    if ctx.array_length(arr) < 64 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short buffer"));
    }
    let mut out = [0u8; 64];
    if ctx.read_byte_array_into(arr, 0, &mut out) != 64 {
        return Err(bc_whirlpool_bad_state(
            "WhirlpoolDigest: failed to read buffer",
        ));
    }
    Ok(out)
}

pub(crate) fn bc_whirlpool_write_buffer(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    buffer: &[u8; 64],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_buffer")?;
    if ctx.array_length(arr) < 64 || !ctx.write_byte_array_from(arr, 0, buffer) {
        return Err(bc_whirlpool_bad_state(
            "WhirlpoolDigest: failed to write buffer",
        ));
    }
    Ok(())
}

pub(crate) fn bc_whirlpool_read_bit_count(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[u8; 32], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_bitCount")?;
    if ctx.array_length(arr) < 32 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short bit count"));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u8,
            _ => return Err(bc_whirlpool_bad_state("WhirlpoolDigest: bad bit count")),
        }
    }
    Ok(out)
}

pub(crate) fn bc_whirlpool_write_bit_count(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    bit_count: &[u8; 32],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_bitCount")?;
    if ctx.array_length(arr) < 32 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short bit count"));
    }
    for (i, &value) in bit_count.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(value as i32));
    }
    Ok(())
}

pub(crate) fn bc_whirlpool_add_bits(bit_count: &mut [u8; 32], bytes: usize) {
    let mut carry = (bytes as u128) * 8;
    for slot in bit_count.iter_mut().rev() {
        let sum = (*slot as u128) + (carry & 0xff);
        *slot = sum as u8;
        carry = (carry >> 8) + (sum >> 8);
        if carry == 0 {
            break;
        }
    }
}

pub(crate) fn bc_whirlpool_pack_block(buffer: &[u8; 64]) -> [u64; 8] {
    let mut block = [0u64; 8];
    for (i, slot) in block.iter_mut().enumerate() {
        let off = i * 8;
        *slot = u64::from_be_bytes([
            buffer[off],
            buffer[off + 1],
            buffer[off + 2],
            buffer[off + 3],
            buffer[off + 4],
            buffer[off + 5],
            buffer[off + 6],
            buffer[off + 7],
        ]);
    }
    block
}

pub(crate) fn bc_whirlpool_process_block_inner(
    hash: &mut [u64; 8],
    k: &mut [u64; 8],
    l: &mut [u64; 8],
    block: &[u64; 8],
    state: &mut [u64; 8],
) {
    let tables = bc_whirlpool_tables();
    for i in 0..8 {
        k[i] = hash[i];
        state[i] = block[i] ^ k[i];
    }

    for round in 1..=10 {
        for i in 0..8 {
            l[i] = tables.c[0][((k[i] >> 56) & 0xff) as usize]
                ^ tables.c[1][((k[(i + 7) & 7] >> 48) & 0xff) as usize]
                ^ tables.c[2][((k[(i + 6) & 7] >> 40) & 0xff) as usize]
                ^ tables.c[3][((k[(i + 5) & 7] >> 32) & 0xff) as usize]
                ^ tables.c[4][((k[(i + 4) & 7] >> 24) & 0xff) as usize]
                ^ tables.c[5][((k[(i + 3) & 7] >> 16) & 0xff) as usize]
                ^ tables.c[6][((k[(i + 2) & 7] >> 8) & 0xff) as usize]
                ^ tables.c[7][(k[(i + 1) & 7] & 0xff) as usize];
        }
        *k = *l;
        k[0] ^= tables.rc[round];

        for i in 0..8 {
            l[i] = k[i]
                ^ tables.c[0][((state[i] >> 56) & 0xff) as usize]
                ^ tables.c[1][((state[(i + 7) & 7] >> 48) & 0xff) as usize]
                ^ tables.c[2][((state[(i + 6) & 7] >> 40) & 0xff) as usize]
                ^ tables.c[3][((state[(i + 5) & 7] >> 32) & 0xff) as usize]
                ^ tables.c[4][((state[(i + 4) & 7] >> 24) & 0xff) as usize]
                ^ tables.c[5][((state[(i + 3) & 7] >> 16) & 0xff) as usize]
                ^ tables.c[6][((state[(i + 2) & 7] >> 8) & 0xff) as usize]
                ^ tables.c[7][(state[(i + 1) & 7] & 0xff) as usize];
        }
        *state = *l;
    }

    for i in 0..8 {
        hash[i] ^= state[i] ^ block[i];
    }
}

pub(crate) fn bc_whirlpool_write_digest_state(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    hash: &[u64; 8],
    k: &[u64; 8],
    l: &[u64; 8],
    block: &[u64; 8],
    state: &[u64; 8],
) -> Result<(), MethodCallFailed> {
    bc_whirlpool_write_long8(ctx, digest, "_hash", hash)?;
    bc_whirlpool_write_long8(ctx, digest, "_K", k)?;
    bc_whirlpool_write_long8(ctx, digest, "_L", l)?;
    bc_whirlpool_write_long8(ctx, digest, "_block", block)?;
    bc_whirlpool_write_long8(ctx, digest, "_state", state)?;
    Ok(())
}

pub(crate) fn bc_whirlpool_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut hash = bc_whirlpool_read_long8(ctx, this, "_hash")?;
    let mut k = bc_whirlpool_read_long8(ctx, this, "_K")?;
    let mut l = bc_whirlpool_read_long8(ctx, this, "_L")?;
    let block = bc_whirlpool_read_long8(ctx, this, "_block")?;
    let mut state = bc_whirlpool_read_long8(ctx, this, "_state")?;
    bc_whirlpool_process_block_inner(&mut hash, &mut k, &mut l, &block, &mut state);
    bc_whirlpool_write_digest_state(ctx, this, &hash, &k, &l, &block, &state)?;
    Ok(None)
}

pub(crate) fn bc_whirlpool_native_update_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len_i <= 0 {
        return Ok(None);
    }
    if in_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(in_off_i).into());
    }
    let in_off = in_off_i as usize;
    let len = len_i as usize;
    if in_off.saturating_add(len) > ctx.array_length(input) {
        return Err(RuntimeError::aioobe_index_only(
            in_off.saturating_add(len).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    let mut data = vec![0u8; len];
    ctx.read_byte_array_into(input, in_off, &mut data);

    let mut buffer = bc_whirlpool_read_buffer(ctx, this)?;
    let mut buffer_pos = match ctx.get_field_by_name(this, "_bufferPos") {
        Value::Int(v) if (0..=64).contains(&v) => v as usize,
        _ => {
            return Err(bc_whirlpool_bad_state(
                "WhirlpoolDigest: bad buffer position",
            ))
        }
    };
    let mut bit_count = bc_whirlpool_read_bit_count(ctx, this)?;
    let mut hash = bc_whirlpool_read_long8(ctx, this, "_hash")?;
    let mut k = bc_whirlpool_read_long8(ctx, this, "_K")?;
    let mut l = bc_whirlpool_read_long8(ctx, this, "_L")?;
    let mut block = bc_whirlpool_read_long8(ctx, this, "_block")?;
    let mut state = bc_whirlpool_read_long8(ctx, this, "_state")?;

    let mut pos = 0usize;
    while pos < data.len() {
        let to_copy = (64 - buffer_pos).min(data.len() - pos);
        buffer[buffer_pos..buffer_pos + to_copy].copy_from_slice(&data[pos..pos + to_copy]);
        buffer_pos += to_copy;
        pos += to_copy;

        if buffer_pos == 64 {
            block = bc_whirlpool_pack_block(&buffer);
            bc_whirlpool_process_block_inner(&mut hash, &mut k, &mut l, &block, &mut state);
            buffer_pos = 0;
            buffer = [0u8; 64];
        }
    }

    bc_whirlpool_add_bits(&mut bit_count, len);
    bc_whirlpool_write_buffer(ctx, this, &buffer)?;
    ctx.set_field_by_name(this, "_bufferPos", Value::Int(buffer_pos as i32));
    bc_whirlpool_write_bit_count(ctx, this, &bit_count)?;
    bc_whirlpool_write_digest_state(ctx, this, &hash, &k, &l, &block, &state)?;
    Ok(None)
}

/// Native fast-path for BouncyCastle's table-based Whirlpool digest. Its
/// `update(byte[], int, int)` implementation feeds large inputs through a
/// byte-at-a-time interpreted loop under the BC JIT ban, so the million-'a'
/// vector is dominated by VM dispatch rather than the digest itself.
pub(crate) fn register_bc_whirlpool_digest(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/crypto/digests/WhirlpoolDigest";
    r.register(
        cls,
        "processBlock",
        "()V",
        bc_whirlpool_native_process_block,
    );
    r.register(cls, "update", "([BII)V", bc_whirlpool_native_update_array);

    r.set_category(__prev_cat);
}

pub(crate) fn bc_poly1305_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_poly1305_i32(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<u32, MethodCallFailed> {
    match ctx.get_field_by_name(this, field) {
        Value::Int(v) => Ok(v as u32),
        _ => Err(bc_poly1305_bad_state("Poly1305: malformed integer state")),
    }
}

pub(crate) fn bc_poly1305_set_i32(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: &str,
    value: u32,
) {
    ctx.set_field_by_name(this, field, Value::Int(value as i32));
}

pub(crate) fn bc_poly1305_block(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "currentBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => Ok(o),
        _ => Err(bc_poly1305_bad_state("Poly1305: malformed current block")),
    }
}

pub(crate) fn bc_poly1305_read_block(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<[u8; 16], MethodCallFailed> {
    let arr = bc_poly1305_block(ctx, this)?;
    let mut out = [0u8; 16];
    if ctx.read_byte_array_into(arr, 0, &mut out) != 16 {
        return Err(bc_poly1305_bad_state("Poly1305: failed to read block"));
    }
    Ok(out)
}

pub(crate) fn bc_poly1305_write_block(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    block: &[u8; 16],
) -> Result<(), MethodCallFailed> {
    let arr = bc_poly1305_block(ctx, this)?;
    if !ctx.write_byte_array_from(arr, 0, block) {
        return Err(bc_poly1305_bad_state("Poly1305: failed to write block"));
    }
    Ok(())
}

pub(crate) fn bc_poly1305_load_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<([u32; 5], [u32; 5], [u32; 4]), MethodCallFailed> {
    let h = [
        bc_poly1305_i32(ctx, this, "h0")?,
        bc_poly1305_i32(ctx, this, "h1")?,
        bc_poly1305_i32(ctx, this, "h2")?,
        bc_poly1305_i32(ctx, this, "h3")?,
        bc_poly1305_i32(ctx, this, "h4")?,
    ];
    let r = [
        bc_poly1305_i32(ctx, this, "r0")?,
        bc_poly1305_i32(ctx, this, "r1")?,
        bc_poly1305_i32(ctx, this, "r2")?,
        bc_poly1305_i32(ctx, this, "r3")?,
        bc_poly1305_i32(ctx, this, "r4")?,
    ];
    let k = [
        bc_poly1305_i32(ctx, this, "k0")?,
        bc_poly1305_i32(ctx, this, "k1")?,
        bc_poly1305_i32(ctx, this, "k2")?,
        bc_poly1305_i32(ctx, this, "k3")?,
    ];
    Ok((h, [r[0], r[1], r[2], r[3], r[4]], [k[0], k[1], k[2], k[3]]))
}

pub(crate) fn bc_poly1305_store_h(ctx: &mut dyn NativeContext, this: ObjectRef, h: &[u32; 5]) {
    bc_poly1305_set_i32(ctx, this, "h0", h[0]);
    bc_poly1305_set_i32(ctx, this, "h1", h[1]);
    bc_poly1305_set_i32(ctx, this, "h2", h[2]);
    bc_poly1305_set_i32(ctx, this, "h3", h[3]);
    bc_poly1305_set_i32(ctx, this, "h4", h[4]);
}

pub(crate) fn bc_poly1305_le_u32(block: &[u8; 16], off: usize) -> u64 {
    u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]) as u64
}

pub(crate) fn bc_poly1305_process_block_inner(
    h: &mut [u32; 5],
    r: &[u32; 5],
    s: &[u32; 5],
    block: &[u8; 16],
    full: bool,
) {
    let t0 = bc_poly1305_le_u32(block, 0);
    let t1 = bc_poly1305_le_u32(block, 4);
    let t2 = bc_poly1305_le_u32(block, 8);
    let t3 = bc_poly1305_le_u32(block, 12);

    h[0] = h[0].wrapping_add((t0 & 0x03ff_ffff) as u32);
    h[1] = h[1].wrapping_add((((t1 << 32) | t0) >> 26 & 0x03ff_ffff) as u32);
    h[2] = h[2].wrapping_add((((t2 << 32) | t1) >> 20 & 0x03ff_ffff) as u32);
    h[3] = h[3].wrapping_add((((t3 << 32) | t2) >> 14 & 0x03ff_ffff) as u32);
    h[4] = h[4].wrapping_add((t3 >> 8) as u32);
    if full {
        h[4] = h[4].wrapping_add(1 << 24);
    }

    let m = |a: u32, b: u32| -> u64 { (a as u64) * (b as u64) };
    let tp0 = m(h[0], r[0]) + m(h[1], s[4]) + m(h[2], s[3]) + m(h[3], s[2]) + m(h[4], s[1]);
    let mut tp1 = m(h[0], r[1]) + m(h[1], r[0]) + m(h[2], s[4]) + m(h[3], s[3]) + m(h[4], s[2]);
    let mut tp2 = m(h[0], r[2]) + m(h[1], r[1]) + m(h[2], r[0]) + m(h[3], s[4]) + m(h[4], s[3]);
    let mut tp3 = m(h[0], r[3]) + m(h[1], r[2]) + m(h[2], r[1]) + m(h[3], r[0]) + m(h[4], s[4]);
    let mut tp4 = m(h[0], r[4]) + m(h[1], r[3]) + m(h[2], r[2]) + m(h[3], r[1]) + m(h[4], r[0]);

    h[0] = (tp0 as u32) & 0x03ff_ffff;
    tp1 += tp0 >> 26;
    h[1] = (tp1 as u32) & 0x03ff_ffff;
    tp2 += tp1 >> 26;
    h[2] = (tp2 as u32) & 0x03ff_ffff;
    tp3 += tp2 >> 26;
    h[3] = (tp3 as u32) & 0x03ff_ffff;
    tp4 += tp3 >> 26;
    h[4] = (tp4 as u32) & 0x03ff_ffff;
    h[0] = h[0].wrapping_add(((tp4 >> 26) as u32).wrapping_mul(5));
    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;
}

pub(crate) fn bc_poly1305_native_update(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len_i <= 0 {
        return Ok(None);
    }
    if in_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(in_off_i).into());
    }
    let in_off = in_off_i as usize;
    let len = len_i as usize;
    if in_off.saturating_add(len) > ctx.array_length(input) {
        return Err(RuntimeError::aioobe_index_only(
            in_off.saturating_add(len).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    let mut data = vec![0u8; len];
    ctx.read_byte_array_into(input, in_off, &mut data);
    let (mut h, r, k) = bc_poly1305_load_state(ctx, this)?;
    let s = [
        0,
        r[1].wrapping_mul(5),
        r[2].wrapping_mul(5),
        r[3].wrapping_mul(5),
        r[4].wrapping_mul(5),
    ];
    let mut block = bc_poly1305_read_block(ctx, this)?;
    let mut block_off = match ctx.get_field_by_name(this, "currentBlockOffset") {
        Value::Int(v) if (0..=16).contains(&v) => v as usize,
        _ => return Err(bc_poly1305_bad_state("Poly1305: bad block offset")),
    };

    let mut pos = 0usize;
    while pos < data.len() {
        if block_off == 16 {
            bc_poly1305_process_block_inner(&mut h, &r, &s, &block, true);
            block_off = 0;
        }
        let to_copy = (16 - block_off).min(data.len() - pos);
        block[block_off..block_off + to_copy].copy_from_slice(&data[pos..pos + to_copy]);
        block_off += to_copy;
        pos += to_copy;
        if block_off == 16 {
            bc_poly1305_process_block_inner(&mut h, &r, &s, &block, true);
            block_off = 0;
        }
    }

    let _ = k;
    bc_poly1305_store_h(ctx, this, &h);
    bc_poly1305_write_block(ctx, this, &block)?;
    ctx.set_field_by_name(this, "currentBlockOffset", Value::Int(block_off as i32));
    Ok(None)
}

pub(crate) fn bc_poly1305_native_do_final(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let out_arr = obj_arg(args, 1)?;
    let out_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if out_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(out_off_i).into());
    }
    let out_off = out_off_i as usize;
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "Output buffer is too short.",
        ));
    }

    let (mut h, r, k) = bc_poly1305_load_state(ctx, this)?;
    let s = [
        0,
        r[1].wrapping_mul(5),
        r[2].wrapping_mul(5),
        r[3].wrapping_mul(5),
        r[4].wrapping_mul(5),
    ];
    let mut block = bc_poly1305_read_block(ctx, this)?;
    let block_off = match ctx.get_field_by_name(this, "currentBlockOffset") {
        Value::Int(v) if (0..=16).contains(&v) => v as usize,
        _ => return Err(bc_poly1305_bad_state("Poly1305: bad block offset")),
    };
    if block_off > 0 {
        if block_off < 16 {
            block[block_off] = 1;
            for b in &mut block[block_off + 1..] {
                *b = 0;
            }
        }
        bc_poly1305_process_block_inner(&mut h, &r, &s, &block, block_off == 16);
    }

    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;
    h[2] = h[2].wrapping_add(h[1] >> 26);
    h[1] &= 0x03ff_ffff;
    h[3] = h[3].wrapping_add(h[2] >> 26);
    h[2] &= 0x03ff_ffff;
    h[4] = h[4].wrapping_add(h[3] >> 26);
    h[3] &= 0x03ff_ffff;
    h[0] = h[0].wrapping_add((h[4] >> 26).wrapping_mul(5));
    h[4] &= 0x03ff_ffff;
    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;

    let mut g0 = h[0].wrapping_add(5);
    let mut b = g0 >> 26;
    g0 &= 0x03ff_ffff;
    let mut g1 = h[1].wrapping_add(b);
    b = g1 >> 26;
    g1 &= 0x03ff_ffff;
    let mut g2 = h[2].wrapping_add(b);
    b = g2 >> 26;
    g2 &= 0x03ff_ffff;
    let mut g3 = h[3].wrapping_add(b);
    b = g3 >> 26;
    g3 &= 0x03ff_ffff;
    let g4 = h[4].wrapping_add(b).wrapping_sub(1 << 26);

    let mask = (g4 >> 31).wrapping_sub(1);
    let nmask = !mask;
    h[0] = (h[0] & nmask) | (g0 & mask);
    h[1] = (h[1] & nmask) | (g1 & mask);
    h[2] = (h[2] & nmask) | (g2 & mask);
    h[3] = (h[3] & nmask) | (g3 & mask);
    h[4] = (h[4] & nmask) | (g4 & mask);

    let mut f0 = (((h[0] | (h[1] << 26)) as u64) & 0xffff_ffff) + k[0] as u64;
    let mut f1 = ((((h[1] >> 6) | (h[2] << 20)) as u64) & 0xffff_ffff) + k[1] as u64;
    let mut f2 = ((((h[2] >> 12) | (h[3] << 14)) as u64) & 0xffff_ffff) + k[2] as u64;
    let mut f3 = ((((h[3] >> 18) | (h[4] << 8)) as u64) & 0xffff_ffff) + k[3] as u64;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&(f0 as u32).to_le_bytes());
    f1 += f0 >> 32;
    out[4..8].copy_from_slice(&(f1 as u32).to_le_bytes());
    f2 += f1 >> 32;
    out[8..12].copy_from_slice(&(f2 as u32).to_le_bytes());
    f3 += f2 >> 32;
    out[12..16].copy_from_slice(&(f3 as u32).to_le_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);

    for field in ["h0", "h1", "h2", "h3", "h4"] {
        bc_poly1305_set_i32(ctx, this, field, 0);
    }
    ctx.set_field_by_name(this, "currentBlockOffset", Value::Int(0));
    Ok(Some(Value::Int(16)))
}

/// Native Poly1305 accumulator/update/finalization. BC's key setup remains in
/// Java, but the block arithmetic and byte buffering are hot under the package
/// JIT ban and feed both standalone Poly1305 and ChaCha20-Poly1305 tests.
pub(crate) fn register_bc_poly1305(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let cls = "org/bouncycastle/crypto/macs/Poly1305";
    r.register(cls, "update", "([BII)V", bc_poly1305_native_update);
    r.register(cls, "doFinal", "([BI)I", bc_poly1305_native_do_final);
    r.set_category(__prev_cat);
}

pub(crate) fn bc_digest_random_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_digest_random_aioobe(index: i32) -> MethodCallFailed {
    RuntimeError::aioobe_index_only(index).into()
}

pub(crate) fn bc_digest_random_range(
    ctx: &dyn NativeContext,
    bytes: ObjectRef,
    start: i32,
    len: i32,
) -> Result<(usize, usize), MethodCallFailed> {
    if start < 0 {
        return Err(bc_digest_random_aioobe(start));
    }
    if len < 0 {
        return Err(bc_digest_random_aioobe(len));
    }

    let start = start as usize;
    let len = len as usize;
    let arr_len = ctx.array_length(bytes);
    let Some(end) = start.checked_add(len) else {
        return Err(bc_digest_random_aioobe(i32::MAX));
    };
    if end > arr_len {
        return Err(bc_digest_random_aioobe(end.min(i32::MAX as usize) as i32));
    }
    Ok((start, len))
}

pub(crate) fn bc_digest_random_object_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: malformed object field",
        )),
    }
}

pub(crate) fn bc_digest_random_long_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<i64, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Long(v) => Ok(v),
        _ => Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: malformed counter field",
        )),
    }
}

pub(crate) fn bc_i32_field(ctx: &dyn NativeContext, obj: ObjectRef, field: &str) -> Option<i32> {
    match ctx.get_field_by_name(obj, field) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

pub(crate) fn bc_i32_field_is(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
    expected: u32,
) -> bool {
    bc_i32_field(ctx, obj, field) == Some(expected as i32)
}

pub(crate) fn bc_digest_general_is_reset(ctx: &dyn NativeContext, digest: ObjectRef) -> bool {
    bc_i32_field(ctx, digest, "xBufOff") == Some(0)
        && matches!(ctx.get_field_by_name(digest, "byteCount"), Value::Long(0))
}

pub(crate) fn bc_digest_fast_kind_if_reset(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Option<BcDigestKind> {
    let class = ctx.class_name_of_id(ctx.class_id_of_object(digest))?;
    if !bc_digest_general_is_reset(ctx, digest) {
        return None;
    }

    match class.as_str() {
        "org/bouncycastle/crypto/digests/SHA1Digest"
            if bc_i32_field(ctx, digest, "xOff") == Some(0)
                && bc_i32_field_is(ctx, digest, "H1", 0x6745_2301)
                && bc_i32_field_is(ctx, digest, "H2", 0xefcd_ab89)
                && bc_i32_field_is(ctx, digest, "H3", 0x98ba_dcfe)
                && bc_i32_field_is(ctx, digest, "H4", 0x1032_5476)
                && bc_i32_field_is(ctx, digest, "H5", 0xc3d2_e1f0) =>
        {
            Some(BcDigestKind::Sha1)
        }
        "org/bouncycastle/crypto/digests/SHA256Digest"
            if bc_i32_field(ctx, digest, "xOff") == Some(0)
                && bc_i32_field_is(ctx, digest, "H1", 0x6a09_e667)
                && bc_i32_field_is(ctx, digest, "H2", 0xbb67_ae85)
                && bc_i32_field_is(ctx, digest, "H3", 0x3c6e_f372)
                && bc_i32_field_is(ctx, digest, "H4", 0xa54f_f53a)
                && bc_i32_field_is(ctx, digest, "H5", 0x510e_527f)
                && bc_i32_field_is(ctx, digest, "H6", 0x9b05_688c)
                && bc_i32_field_is(ctx, digest, "H7", 0x1f83_d9ab)
                && bc_i32_field_is(ctx, digest, "H8", 0x5be0_cd19) =>
        {
            Some(BcDigestKind::Sha256)
        }
        _ => None,
    }
}

pub(crate) fn bc_digest_random_read_bytes(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub(crate) fn bc_digest_random_counter_bytes(counter: i64) -> [u8; 8] {
    (counter as u64).to_le_bytes()
}

pub(crate) fn bc_digest_random_generate_state_fast(
    kind: BcDigestKind,
    state: &mut Vec<u8>,
    seed: &mut Vec<u8>,
    state_counter: &mut i64,
    seed_counter: &mut i64,
) {
    let counter = *state_counter;
    *state_counter = state_counter.wrapping_add(1);

    let mut msg = Vec::with_capacity(8 + state.len() + seed.len());
    msg.extend_from_slice(&bc_digest_random_counter_bytes(counter));
    msg.extend_from_slice(state);
    msg.extend_from_slice(seed);
    *state = kind.digest(&msg);

    if *state_counter % 10 == 0 {
        let counter = *seed_counter;
        *seed_counter = seed_counter.wrapping_add(1);
        let mut msg = Vec::with_capacity(seed.len() + 8);
        msg.extend_from_slice(seed);
        msg.extend_from_slice(&bc_digest_random_counter_bytes(counter));
        *seed = kind.digest(&msg);
    }
}

pub(crate) fn bc_digest_random_next_fast(
    ctx: &mut dyn NativeContext,
    kind: BcDigestKind,
    this: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let state_arr = bc_digest_random_object_field(ctx, this, "state")?;
    let seed_arr = bc_digest_random_object_field(ctx, this, "seed")?;
    if ctx.array_length(state_arr) != kind.len() || ctx.array_length(seed_arr) != kind.len() {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: digest/state length mismatch",
        ));
    }

    let mut state = bc_digest_random_read_bytes(ctx, state_arr);
    let mut seed = bc_digest_random_read_bytes(ctx, seed_arr);
    if state.len() != kind.len() || seed.len() != kind.len() {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: failed to read state arrays",
        ));
    }

    let mut state_counter = bc_digest_random_long_field(ctx, this, "stateCounter")?;
    let mut seed_counter = bc_digest_random_long_field(ctx, this, "seedCounter")?;
    let mut out = vec![0u8; len];
    let mut state_off = 0usize;

    bc_digest_random_generate_state_fast(
        kind,
        &mut state,
        &mut seed,
        &mut state_counter,
        &mut seed_counter,
    );

    for b in &mut out {
        if state_off == state.len() {
            bc_digest_random_generate_state_fast(
                kind,
                &mut state,
                &mut seed,
                &mut state_counter,
                &mut seed_counter,
            );
            state_off = 0;
        }
        *b = state[state_off];
        state_off += 1;
    }

    if !ctx.write_byte_array_from(bytes, start, &out)
        || !ctx.write_byte_array_from(state_arr, 0, &state)
        || !ctx.write_byte_array_from(seed_arr, 0, &seed)
    {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: failed to write state arrays",
        ));
    }
    ctx.set_field_by_name(this, "stateCounter", Value::Long(state_counter));
    ctx.set_field_by_name(this, "seedCounter", Value::Long(seed_counter));
    Ok(None)
}

pub(crate) fn bc_digest_update_counter_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    counter: i64,
) -> MethodCallResult {
    let mut value = counter as u64;
    // GC-safety: `update` is real BouncyCastle bytecode -- it allocates, and a
    // moving young collection relocates `digest`. `digest` is a bare Rust local
    // carried into all EIGHT turns of this loop, so from turn two on the
    // dispatch is on a pre-GC address. Pin once, re-read at the top of the
    // body (the shadow leaves the outer binding alone).
    let digest_pin = ctx.pin_native_root(digest);
    for _ in 0..8 {
        let digest = ctx.read_native_pin(digest_pin, digest);
        ctx.invoke_virtual(
            digest,
            "update",
            "(B)V",
            &[Value::Int(value as u8 as i8 as i32)],
        )?;
        value >>= 8;
    }
    Ok(None)
}

pub(crate) fn bc_digest_update_array_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    arr: ObjectRef,
) -> MethodCallResult {
    let len = ctx.array_length(arr);
    ctx.invoke_virtual(
        digest,
        "update",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(len.min(i32::MAX as usize) as i32),
        ],
    )?;
    Ok(None)
}

pub(crate) fn bc_digest_do_final_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    arr: ObjectRef,
) -> MethodCallResult {
    ctx.invoke_virtual(
        digest,
        "doFinal",
        "([BI)I",
        &[Value::Object(Some(arr)), Value::Int(0)],
    )?;
    Ok(None)
}

pub(crate) fn bc_digest_random_cycle_seed_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    seed_arr: ObjectRef,
    seed_counter: &mut i64,
) -> MethodCallResult {
    bc_digest_update_array_virtual(ctx, digest, seed_arr)?;
    let counter = *seed_counter;
    *seed_counter = seed_counter.wrapping_add(1);
    ctx.set_field_by_name(this, "seedCounter", Value::Long(*seed_counter));
    bc_digest_update_counter_virtual(ctx, digest, counter)?;
    bc_digest_do_final_virtual(ctx, digest, seed_arr)
}

pub(crate) fn bc_digest_random_generate_state_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    state_arr: ObjectRef,
    seed_arr: ObjectRef,
    state_counter: &mut i64,
    seed_counter: &mut i64,
) -> MethodCallResult {
    let counter = *state_counter;
    *state_counter = state_counter.wrapping_add(1);
    ctx.set_field_by_name(this, "stateCounter", Value::Long(*state_counter));
    bc_digest_update_counter_virtual(ctx, digest, counter)?;
    bc_digest_update_array_virtual(ctx, digest, state_arr)?;
    bc_digest_update_array_virtual(ctx, digest, seed_arr)?;
    bc_digest_do_final_virtual(ctx, digest, state_arr)?;

    if *state_counter % 10 == 0 {
        bc_digest_random_cycle_seed_virtual(ctx, this, digest, seed_arr, seed_counter)?;
    }
    Ok(None)
}

pub(crate) fn bc_digest_random_next_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let state_arr = bc_digest_random_object_field(ctx, this, "state")?;
    let seed_arr = bc_digest_random_object_field(ctx, this, "seed")?;
    let state_len = ctx.array_length(state_arr);
    if state_len == 0 {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: empty state",
        ));
    }

    let mut state_counter = bc_digest_random_long_field(ctx, this, "stateCounter")?;
    let mut seed_counter = bc_digest_random_long_field(ctx, this, "seedCounter")?;
    let mut state_off = 0usize;

    bc_digest_random_generate_state_virtual(
        ctx,
        this,
        digest,
        state_arr,
        seed_arr,
        &mut state_counter,
        &mut seed_counter,
    )?;

    // GC-safety: `bc_digest_random_generate_state_virtual` dispatches
    // `Digest.update`/`doFinal` -- real bytecode that allocates -- and it runs
    // INSIDE this loop, once per state refill. Every reference the body carries
    // in from outside (`this`, `digest`, and both byte arrays, one of which is
    // read and one written on EVERY turn) is a bare Rust local nothing
    // rewrites. Pin all four and re-read at the top of the body.
    let this_pin = ctx.pin_native_root(this);
    let digest_pin = ctx.pin_native_root(digest);
    let state_pin = ctx.pin_native_root(state_arr);
    let seed_pin = ctx.pin_native_root(seed_arr);
    let bytes_pin = ctx.pin_native_root(bytes);
    for i in start..start + len {
        let this = ctx.read_native_pin(this_pin, this);
        let digest = ctx.read_native_pin(digest_pin, digest);
        let state_arr = ctx.read_native_pin(state_pin, state_arr);
        let seed_arr = ctx.read_native_pin(seed_pin, seed_arr);
        let bytes = ctx.read_native_pin(bytes_pin, bytes);
        if state_off == state_len {
            bc_digest_random_generate_state_virtual(
                ctx,
                this,
                digest,
                state_arr,
                seed_arr,
                &mut state_counter,
                &mut seed_counter,
            )?;
            state_off = 0;
        }
        // Re-read once more: the refill above ran bytecode.
        let state_arr = ctx.read_native_pin(state_pin, state_arr);
        let bytes = ctx.read_native_pin(bytes_pin, bytes);
        let v = ctx.get_array_element(state_arr, state_off);
        ctx.set_array_element(bytes, i, v);
        state_off += 1;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn bc_digest_random_next_locked(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let digest = bc_digest_random_object_field(ctx, this, "digest")?;
    if let Some(kind) = bc_digest_fast_kind_if_reset(ctx, digest) {
        return bc_digest_random_next_fast(ctx, kind, this, bytes, start, len);
    }
    bc_digest_random_next_virtual(ctx, this, digest, bytes, start, len)
}

/// Native fast-path for BouncyCastle's synchronized
/// `DigestRandomGenerator.nextBytes`. The generator is hot in
/// `DigestRandomNumberTest`: two count tests call `nextBytes(byte[])` one
/// million times each. The x64 JIT intentionally bails on non-elided
/// monitorenter/monitorexit bytecode, so the synchronized three-arg body stayed
/// interpreted even when `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/` was
/// used to investigate BC JIT correctness. This intrinsic preserves the object
/// monitor via the VM's monitor table and mirrors the BC state transitions.
pub(crate) fn register_bc_digest_random_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/crypto/prng/DigestRandomGenerator";

    r.register(cls, "nextBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = obj_arg(args, 1)?;
        let len = ctx.array_length(bytes).min(i32::MAX as usize) as i32;
        ctx.monitor_enter(this);
        let result = bc_digest_random_next_locked(ctx, this, bytes, 0, len as usize);
        ctx.monitor_exit(this);
        result
    });

    r.register(cls, "nextBytes", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = obj_arg(args, 1)?;
        let start = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let (start, len) = bc_digest_random_range(ctx, bytes, start, len)?;
        ctx.monitor_enter(this);
        let result = bc_digest_random_next_locked(ctx, this, bytes, start, len);
        ctx.monitor_exit(this);
        result
    });

    r.set_category(__prev_cat);
}

pub(crate) fn register_bc_primes_small_factors(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    // BC's RSA-keygen primality pre-screen (`Primes.isProbablePrime` path).
    r.register(
        "org/bouncycastle/math/Primes",
        "implHasAnySmallFactors",
        "(Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let x = bi_read_int(ctx, obj_arg(args, 0)?);
            let has = bc_has_any_small_factors(x.mag_le(), x.is_neg());
            Ok(Some(Value::Int(i32::from(has))))
        },
    );

    r.register(
        "org/bouncycastle/math/Primes",
        "isMRProbablePrime",
        "(Ljava/math/BigInteger;Ljava/security/SecureRandom;I)Z",
        |ctx, args| {
            let candidate = bc_primes_candidate_arg(ctx, args, 0, "candidate")?;
            if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'random' cannot be null".into(),
                }
                .into());
            }
            let iterations = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            if iterations < 1 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'iterations' must be > 0".into(),
                }
                .into());
            }
            Ok(Some(Value::Int(i32::from(candidate.is_probable_prime()))))
        },
    );

    r.register(
        "org/bouncycastle/math/Primes",
        "isMRProbablePrimeToBase",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let candidate = bc_primes_candidate_arg(ctx, args, 0, "candidate")?;
            let base = bc_primes_candidate_arg(ctx, args, 1, "base")?;
            let max_base = candidate.sub(&bc_bigint_small(1));
            if base.cmp(&max_base) != std::cmp::Ordering::Less {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'base' must be < ('candidate' - 1)".into(),
                }
                .into());
            }
            Ok(Some(Value::Int(i32::from(bc_mr_probable_prime_to_base(
                &candidate, &base,
            )))))
        },
    );

    // BC's `BigIntegers.createRandomPrime` initial-candidate sieve, which
    // otherwise runs the interpreted safegcd `Mod.modOddIsCoprimeVar /
    // updateFG30` against SMALL_PRIMES_PRODUCT — the dominant cost once
    // implHasAnySmallFactors above is native. Equivalent single-word-mod sieve.
    r.register(
        "org/bouncycastle/util/BigIntegers",
        "hasAnySmallFactors",
        "(Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let x = bi_read_int(ctx, obj_arg(args, 0)?);
            let has = bc_util_has_any_small_factors(x.mag_le());
            Ok(Some(Value::Int(i32::from(has))))
        },
    );
    // Native bridge for BC's safegcd inverse wrappers. The raw
    // org/bouncycastle/math/raw/Mod path is part of the documented
    // cross-package JIT hazard; use the VM's already-tested BigInteger inverse
    // core instead of entering that interpreted/JIT-sensitive loop.
    r.register(
        "org/bouncycastle/util/BigIntegers",
        "modOddInverse",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| bc_big_integers_mod_odd_inverse(ctx, args, false),
    );
    r.register(
        "org/bouncycastle/util/BigIntegers",
        "modOddInverseVar",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| bc_big_integers_mod_odd_inverse(ctx, args, true),
    );

    r.set_category(__prev_cat);
}

/// Read BouncyCastle's expanded AES key schedule (`int[][] KW`) into
/// `Vec<[u32; 4]>` — `KW[round][col]`.
pub(crate) fn read_aes_kw(ctx: &dyn NativeContext, outer: ObjectRef) -> Vec<[u32; 4]> {
    let rows = ctx.array_length(outer);
    let mut kw = Vec::with_capacity(rows);
    for r in 0..rows {
        let row = match ctx.get_array_element(outer, r) {
            Value::Object(Some(o)) => o,
            _ => return Vec::new(),
        };
        let mut cols = [0u32; 4];
        for (c, slot) in cols.iter_mut().enumerate() {
            *slot = match ctx.get_array_element(row, c) {
                Value::Int(v) => v as u32,
                _ => 0,
            };
        }
        kw.push(cols);
    }
    kw
}

pub(crate) fn bc_aes_bad_key() -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: "Key length not 128/192/256 bits.".into(),
    }
    .into()
}

pub(crate) fn bc_aes_alloc_working_key(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_arr: ObjectRef,
    for_enc: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let klen = ctx.array_length(key_arr);
    let mut kbuf = [0u8; 32];
    let key: &[u8] = if klen <= 32 {
        let n = ctx.read_byte_array_into(key_arr, 0, &mut kbuf[..klen]);
        &kbuf[..n]
    } else {
        &[]
    };

    let w = crate::bc_aes::generate_working_key(key, for_enc).ok_or_else(bc_aes_bad_key)?;

    ctx.set_field_by_name(this, "ROUNDS", Value::Int((w.len() - 1) as i32));

    let outer = ctx.new_array(cratonvm_types::ArrayElementType::Reference, w.len());
    for (r_idx, cols) in w.iter().enumerate() {
        let row = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        for (c, &word) in cols.iter().enumerate() {
            ctx.set_array_element(row, c, Value::Int(word as i32));
        }
        ctx.set_array_element(outer, r_idx, Value::Object(Some(row)));
    }
    Ok(outer)
}

pub(crate) fn bc_aes_clone_static_sbox(
    ctx: &mut dyn NativeContext,
    aes_class: &str,
    field_name: &str,
) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name(aes_class)?;
    let field_index = ctx.static_field_index_by_name(class_id, field_name)?;
    let Value::Object(Some(src)) = ctx.get_static_field(class_id, field_index) else {
        return None;
    };
    let len = ctx.array_length(src);
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    let mut bytes = vec![0u8; len];
    ctx.read_byte_array_into(src, 0, &mut bytes);
    ctx.write_byte_array_from(dst, 0, &bytes);
    Some(dst)
}

pub(crate) const BC_GOST_PI: [u8; 256] = [
    252, 238, 221, 17, 207, 110, 49, 22, 251, 196, 250, 218, 35, 197, 4, 77, 233, 119, 240, 219,
    147, 46, 153, 186, 23, 54, 241, 187, 20, 205, 95, 193, 249, 24, 101, 90, 226, 92, 239, 33, 129,
    28, 60, 66, 139, 1, 142, 79, 5, 132, 2, 174, 227, 106, 143, 160, 6, 11, 237, 152, 127, 212,
    211, 31, 235, 52, 44, 81, 234, 200, 72, 171, 242, 42, 104, 162, 253, 58, 206, 204, 181, 112,
    14, 86, 8, 12, 118, 18, 191, 114, 19, 71, 156, 183, 93, 135, 21, 161, 150, 41, 16, 123, 154,
    199, 243, 145, 120, 111, 157, 158, 178, 177, 50, 117, 25, 61, 255, 53, 138, 126, 109, 84, 198,
    128, 195, 189, 13, 87, 223, 245, 36, 169, 62, 168, 67, 201, 215, 121, 214, 246, 124, 34, 185,
    3, 224, 15, 236, 222, 122, 148, 176, 188, 220, 232, 40, 80, 78, 51, 10, 74, 167, 151, 96, 115,
    30, 0, 98, 68, 26, 184, 56, 130, 100, 159, 38, 65, 173, 69, 70, 146, 39, 94, 85, 47, 140, 163,
    165, 125, 105, 213, 149, 59, 7, 88, 179, 64, 134, 172, 29, 247, 48, 55, 107, 228, 136, 217,
    231, 137, 225, 27, 131, 73, 76, 63, 248, 254, 141, 83, 170, 144, 202, 216, 133, 97, 32, 113,
    103, 164, 45, 43, 9, 91, 203, 155, 37, 208, 190, 229, 108, 82, 89, 166, 116, 210, 230, 244,
    180, 192, 209, 102, 175, 194, 57, 75, 99, 182,
];

pub(crate) const BC_GOST_INV_PI: [u8; 256] = [
    165, 45, 50, 143, 14, 48, 56, 192, 84, 230, 158, 57, 85, 126, 82, 145, 100, 3, 87, 90, 28, 96,
    7, 24, 33, 114, 168, 209, 41, 198, 164, 63, 224, 39, 141, 12, 130, 234, 174, 180, 154, 99, 73,
    229, 66, 228, 21, 183, 200, 6, 112, 157, 65, 117, 25, 201, 170, 252, 77, 191, 42, 115, 132,
    213, 195, 175, 43, 134, 167, 177, 178, 91, 70, 211, 159, 253, 212, 15, 156, 47, 155, 67, 239,
    217, 121, 182, 83, 127, 193, 240, 35, 231, 37, 94, 181, 30, 162, 223, 166, 254, 172, 34, 249,
    226, 74, 188, 53, 202, 238, 120, 5, 107, 81, 225, 89, 163, 242, 113, 86, 17, 106, 137, 148,
    101, 140, 187, 119, 60, 123, 40, 171, 210, 49, 222, 196, 95, 204, 207, 118, 44, 184, 216, 46,
    54, 219, 105, 179, 20, 149, 190, 98, 161, 59, 22, 102, 233, 92, 108, 109, 173, 55, 97, 75, 185,
    227, 186, 241, 160, 133, 131, 218, 71, 197, 176, 51, 250, 150, 111, 110, 194, 246, 80, 255, 93,
    169, 142, 23, 27, 151, 125, 236, 88, 247, 31, 251, 124, 9, 13, 122, 103, 69, 135, 220, 232, 79,
    29, 78, 4, 235, 248, 243, 62, 61, 189, 138, 136, 221, 205, 11, 19, 152, 2, 147, 128, 144, 208,
    36, 52, 203, 237, 244, 206, 153, 16, 68, 64, 146, 58, 1, 38, 18, 26, 72, 104, 245, 129, 139,
    199, 214, 32, 10, 8, 0, 76, 215, 116,
];

pub(crate) const BC_GOST_L_FACTORS: [u8; 16] = [
    148, 32, 133, 16, 194, 192, 1, 251, 1, 192, 194, 16, 133, 32, 148, 1,
];

pub(crate) fn bc_gost_mul_slow(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if a == 0 || b == 0 {
            break;
        }
        if (b & 1) != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a = a.wrapping_shl(1);
        if hi != 0 {
            a ^= 0xc3;
        }
        b = ((b as i8) >> 1) as u8;
    }
    p
}

pub(crate) fn bc_gost_lfactor_table() -> &'static [[u8; 256]; 16] {
    static TABLE: std::sync::OnceLock<[[u8; 256]; 16]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [[0u8; 256]; 16];
        for (i, &factor) in BC_GOST_L_FACTORS.iter().enumerate() {
            for x in 0..256 {
                table[i][x] = bc_gost_mul_slow(x as u8, factor);
            }
        }
        table
    })
}

pub(crate) fn bc_gost_l(data: &[u8; 16]) -> u8 {
    let table = bc_gost_lfactor_table();
    let mut x = data[15];
    for i in (0..15).rev() {
        x ^= table[i][data[i] as usize];
    }
    x
}

pub(crate) fn bc_gost_r(data: &mut [u8; 16]) {
    let z = bc_gost_l(data);
    data.copy_within(0..15, 1);
    data[0] = z;
}

pub(crate) fn bc_gost_inverse_r(data: &mut [u8; 16]) {
    let mut temp = [0u8; 16];
    temp[..15].copy_from_slice(&data[1..]);
    temp[15] = data[0];
    let z = bc_gost_l(&temp);
    data.copy_within(1..16, 0);
    data[15] = z;
}

pub(crate) fn bc_gost_l_transform(data: &mut [u8; 16]) {
    for _ in 0..16 {
        bc_gost_r(data);
    }
}

pub(crate) fn bc_gost_inverse_l_transform(data: &mut [u8; 16]) {
    for _ in 0..16 {
        bc_gost_inverse_r(data);
    }
}

pub(crate) fn bc_gost_x(data: &mut [u8; 16], rhs: &[u8; 16]) {
    for i in 0..16 {
        data[i] ^= rhs[i];
    }
}

pub(crate) fn bc_gost_lsx(k: &[u8; 16], a: &[u8; 16]) -> [u8; 16] {
    let mut result = *k;
    bc_gost_x(&mut result, a);
    for b in &mut result {
        *b = BC_GOST_PI[*b as usize];
    }
    bc_gost_l_transform(&mut result);
    result
}

pub(crate) fn bc_gost_xsl(k: &[u8; 16], a: &[u8; 16]) -> [u8; 16] {
    let mut result = *k;
    bc_gost_x(&mut result, a);
    bc_gost_inverse_l_transform(&mut result);
    for b in &mut result {
        *b = BC_GOST_INV_PI[*b as usize];
    }
    result
}

pub(crate) fn bc_gost_read_subkeys(
    ctx: &dyn NativeContext,
    subkeys_outer: ObjectRef,
) -> Option<[[u8; 16]; 10]> {
    if ctx.array_length(subkeys_outer) != 10 {
        return None;
    }
    let mut subkeys = [[0u8; 16]; 10];
    for (i, slot) in subkeys.iter_mut().enumerate() {
        let row = match ctx.get_array_element(subkeys_outer, i) {
            Value::Object(Some(o)) => o,
            _ => return None,
        };
        if ctx.array_length(row) != 16 || ctx.read_byte_array_into(row, 0, slot) != 16 {
            return None;
        }
    }
    Some(subkeys)
}

pub(crate) fn bc_gost_process_block(
    subkeys: &[[u8; 16]; 10],
    for_encryption: bool,
    input: &[u8; 16],
) -> [u8; 16] {
    let mut block = *input;
    if for_encryption {
        for key in subkeys.iter().take(9) {
            block = bc_gost_lsx(key, &block);
        }
        bc_gost_x(&mut block, &subkeys[9]);
    } else {
        for key in subkeys.iter().take(10).skip(1).rev() {
            block = bc_gost_xsl(key, &block);
        }
        bc_gost_x(&mut block, &subkeys[0]);
    }
    block
}

pub(crate) fn bc_gost_throw_crypto_exception(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    message: &str,
) -> MethodCallFailed {
    let msg = ctx.create_string(message);
    match ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalStateException {
            message: format!("{class_name}: {message}"),
        }
        .into(),
    }
}

pub(crate) fn bc_aes_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_aes_block_args(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<([u8; 16], ObjectRef, usize, Vec<[u32; 4]>)> {
    let in_arr = obj_arg(args, 1).ok()?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return None,
    };
    let out_arr = obj_arg(args, 3).ok()?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return None,
    };
    let kw = read_aes_kw(ctx, obj_arg(args, 5).ok()?);
    if kw.len() < 2 {
        return None;
    }
    let mut inb = [0u8; 16];
    if ctx.read_byte_array_into(in_arr, in_off, &mut inb) != 16 {
        return None;
    }
    Some((inb, out_arr, out_off, kw))
}

pub(crate) fn bc_aes_native_encrypt_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (inb, out_arr, out_off, kw) = bc_aes_block_args(ctx, args)
        .ok_or_else(|| bc_aes_bad_state("AESEngine native: malformed block/key state"))?;
    let mut outb = [0u8; 16];
    crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(None)
}

pub(crate) fn bc_aes_native_decrypt_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (inb, out_arr, out_off, kw) = bc_aes_block_args(ctx, args)
        .ok_or_else(|| bc_aes_bad_state("AESEngine native: malformed block/key state"))?;
    let mut outb = [0u8; 16];
    crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(None)
}

pub(crate) fn bc_aes_native_generate_working_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key_arr = obj_arg(args, 1)?;
    let for_enc = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
    Ok(Some(Value::Object(Some(wk))))
}

/// Are BouncyCastle services constraints installed?
///
/// Every AES engine entry point that CratonVM replaces natively ends, in
/// BouncyCastle's own bytecode, with a
/// `CryptoServicesRegistrar.checkConstraints(...)` call: the constructors
/// check the algorithm at full strength, `init` checks the key that was
/// actually supplied. That call raises `CryptoServiceConstraintsException`
/// when the process has constraints installed and the service does not meet
/// them, and a native that REPLACES the whole method drops it — an
/// under-strength key then initialises with no error at all, which is exactly
/// the failure a constraints policy exists to prevent
/// (`SymmetricConstraintsTest.testAES`, "no exception!").
///
/// Constraints are off by default and these natives exist for that default
/// path, so ask the registrar and hand the call straight back to the bytecode
/// whenever anything other than the built-in no-op constraints object is
/// installed. The real method then performs the real check with BouncyCastle's
/// own `DefaultServiceProperties`, which carry a per-engine bits-of-security
/// figure and a purpose derived from the direction — not something worth
/// transcribing here, where it would silently rot against the library.
///
/// Every uncertain answer (registrar unloadable, field renamed, accessor
/// missing) reports `true`: declining to the bytecode is always correct, only
/// slower, whereas skipping the check is a security hole.
fn bc_services_constraints_active(ctx: &mut dyn NativeContext) -> bool {
    const REGISTRAR: &str = "org/bouncycastle/crypto/CryptoServicesRegistrar";
    let Ok(class_id) = ctx.ensure_class_initialized(REGISTRAR) else {
        return true;
    };
    let Some(idx) = ctx.static_field_index_by_name(class_id, "noConstraintsImpl") else {
        return true;
    };
    let Value::Object(no_constraints) = ctx.get_static_field(class_id, idx) else {
        return true;
    };
    match ctx.invoke(
        REGISTRAR,
        "getServicesConstraints",
        "()Lorg/bouncycastle/crypto/CryptoServicesConstraints;",
        &[],
    ) {
        Ok(Some(Value::Object(current))) => current != no_constraints,
        _ => true,
    }
}

/// `<init>()` for the three AES engines. BouncyCastle's constructors are not
/// empty: each one checks its algorithm against the installed constraints at
/// full strength (`AESEngine` hardcodes 256, the other two ask
/// `bitsOfSecurity()`). With no constraints installed there is genuinely
/// nothing to do — the key schedule is built lazily by `init` /
/// `generateWorkingKey`, and no field initialiser runs — so the native keeps
/// the empty fast path that `newInstance()`'s `alloc_object` path wants, and
/// defers to the bytecode only when the check can actually fire.
pub(crate) fn bc_aes_native_ctor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bc_services_constraints_active(ctx) {
        let Some(class_name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(this)) else {
            return Ok(None);
        };
        return ctx.invoke_special_bytecode_only(
            &class_name,
            "<init>",
            "()V",
            &[Value::Object(Some(this))],
        );
    }
    Ok(None)
}

pub(crate) fn bc_aes_native_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bc_services_constraints_active(ctx) {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "init",
            "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
            &args[1..],
        );
    }
    let for_enc = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let params = obj_arg(args, 2)?;
    let key_arr = match ctx.get_field_by_name(params, "key") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "invalid parameter passed to AES init".into(),
            }
            .into())
        }
    };
    let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
    ctx.set_field_by_name(this, "WorkingKey", Value::Object(Some(wk)));
    ctx.set_field_by_name(this, "forEncryption", Value::Int(i32::from(for_enc)));

    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/bouncycastle/crypto/engines/AESEngine")
    {
        if let Some(sbox) = bc_aes_clone_static_sbox(
            ctx,
            "org/bouncycastle/crypto/engines/AESEngine",
            if for_enc { "S" } else { "Si" },
        ) {
            ctx.set_field_by_name(this, "s", Value::Object(Some(sbox)));
        }
    }
    Ok(None)
}

pub(crate) fn bc_aes_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };
    let wk_arr = match ctx.get_field_by_name(this, "WorkingKey") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_aes_bad_state("AES engine not initialised")),
    };
    if in_off.saturating_add(16) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }
    let kw = read_aes_kw(ctx, wk_arr);
    if kw.len() < 2 {
        return Err(bc_aes_bad_state(
            "AESEngine native: malformed block/key state",
        ));
    }
    let mut inb = [0u8; 16];
    ctx.read_byte_array_into(in_arr, in_off, &mut inb);
    let mut outb = [0u8; 16];
    if matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0) {
        crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
    } else {
        crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
    }
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(Some(Value::Int(16)))
}

pub(crate) const BC_SM4_SBOX: [u8; 256] = [
    0xd6, 0x90, 0xe9, 0xfe, 0xcc, 0xe1, 0x3d, 0xb7, 0x16, 0xb6, 0x14, 0xc2, 0x28, 0xfb, 0x2c, 0x05,
    0x2b, 0x67, 0x9a, 0x76, 0x2a, 0xbe, 0x04, 0xc3, 0xaa, 0x44, 0x13, 0x26, 0x49, 0x86, 0x06, 0x99,
    0x9c, 0x42, 0x50, 0xf4, 0x91, 0xef, 0x98, 0x7a, 0x33, 0x54, 0x0b, 0x43, 0xed, 0xcf, 0xac, 0x62,
    0xe4, 0xb3, 0x1c, 0xa9, 0xc9, 0x08, 0xe8, 0x95, 0x80, 0xdf, 0x94, 0xfa, 0x75, 0x8f, 0x3f, 0xa6,
    0x47, 0x07, 0xa7, 0xfc, 0xf3, 0x73, 0x17, 0xba, 0x83, 0x59, 0x3c, 0x19, 0xe6, 0x85, 0x4f, 0xa8,
    0x68, 0x6b, 0x81, 0xb2, 0x71, 0x64, 0xda, 0x8b, 0xf8, 0xeb, 0x0f, 0x4b, 0x70, 0x56, 0x9d, 0x35,
    0x1e, 0x24, 0x0e, 0x5e, 0x63, 0x58, 0xd1, 0xa2, 0x25, 0x22, 0x7c, 0x3b, 0x01, 0x21, 0x78, 0x87,
    0xd4, 0x00, 0x46, 0x57, 0x9f, 0xd3, 0x27, 0x52, 0x4c, 0x36, 0x02, 0xe7, 0xa0, 0xc4, 0xc8, 0x9e,
    0xea, 0xbf, 0x8a, 0xd2, 0x40, 0xc7, 0x38, 0xb5, 0xa3, 0xf7, 0xf2, 0xce, 0xf9, 0x61, 0x15, 0xa1,
    0xe0, 0xae, 0x5d, 0xa4, 0x9b, 0x34, 0x1a, 0x55, 0xad, 0x93, 0x32, 0x30, 0xf5, 0x8c, 0xb1, 0xe3,
    0x1d, 0xf6, 0xe2, 0x2e, 0x82, 0x66, 0xca, 0x60, 0xc0, 0x29, 0x23, 0xab, 0x0d, 0x53, 0x4e, 0x6f,
    0xd5, 0xdb, 0x37, 0x45, 0xde, 0xfd, 0x8e, 0x2f, 0x03, 0xff, 0x6a, 0x72, 0x6d, 0x6c, 0x5b, 0x51,
    0x8d, 0x1b, 0xaf, 0x92, 0xbb, 0xdd, 0xbc, 0x7f, 0x11, 0xd9, 0x5c, 0x41, 0x1f, 0x10, 0x5a, 0xd8,
    0x0a, 0xc1, 0x31, 0x88, 0xa5, 0xcd, 0x7b, 0xbd, 0x2d, 0x74, 0xd0, 0x12, 0xb8, 0xe5, 0xb4, 0xb0,
    0x89, 0x69, 0x97, 0x4a, 0x0c, 0x96, 0x77, 0x7e, 0x65, 0xb9, 0xf1, 0x09, 0xc5, 0x6e, 0xc6, 0x84,
    0x18, 0xf0, 0x7d, 0xec, 0x3a, 0xdc, 0x4d, 0x20, 0x79, 0xee, 0x5f, 0x3e, 0xd7, 0xcb, 0x39, 0x48,
];

pub(crate) fn bc_sm4_t(z: u32) -> u32 {
    let b0 = BC_SM4_SBOX[((z >> 24) & 0xff) as usize] as u32;
    let b1 = BC_SM4_SBOX[((z >> 16) & 0xff) as usize] as u32;
    let b2 = BC_SM4_SBOX[((z >> 8) & 0xff) as usize] as u32;
    let b3 = BC_SM4_SBOX[(z & 0xff) as usize] as u32;
    let b = (b0 << 24) | (b1 << 16) | (b2 << 8) | b3;
    b ^ b.rotate_left(2) ^ b.rotate_left(10) ^ b.rotate_left(18) ^ b.rotate_left(24)
}

pub(crate) fn bc_sm4_read_rk(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<[u32; 32], MethodCallFailed> {
    let rk_arr = match ctx.get_field_by_name(this, "rk") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "SM4 not initialised".into(),
            }
            .into())
        }
    };
    if ctx.array_length(rk_arr) < 32 {
        return Err(RuntimeError::IllegalStateException {
            message: "SM4 native: malformed round-key state".into(),
        }
        .into());
    }
    let mut rk = [0u32; 32];
    for (i, slot) in rk.iter_mut().enumerate() {
        match ctx.get_array_element(rk_arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "SM4 native: malformed round-key state".into(),
                }
                .into())
            }
        }
    }
    Ok(rk)
}

pub(crate) fn bc_sm4_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };
    let rk = bc_sm4_read_rk(ctx, this)?;
    if in_off.saturating_add(16) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let mut block = [0u8; 16];
    ctx.read_byte_array_into(in_arr, in_off, &mut block);
    let mut x = [
        u32::from_be_bytes([block[0], block[1], block[2], block[3]]),
        u32::from_be_bytes([block[4], block[5], block[6], block[7]]),
        u32::from_be_bytes([block[8], block[9], block[10], block[11]]),
        u32::from_be_bytes([block[12], block[13], block[14], block[15]]),
    ];
    for i in (0..32).step_by(4) {
        x[0] ^= bc_sm4_t(x[1] ^ x[2] ^ x[3] ^ rk[i]);
        x[1] ^= bc_sm4_t(x[2] ^ x[3] ^ x[0] ^ rk[i + 1]);
        x[2] ^= bc_sm4_t(x[3] ^ x[0] ^ x[1] ^ rk[i + 2]);
        x[3] ^= bc_sm4_t(x[0] ^ x[1] ^ x[2] ^ rk[i + 3]);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&x[3].to_be_bytes());
    out[4..8].copy_from_slice(&x[2].to_be_bytes());
    out[8..12].copy_from_slice(&x[1].to_be_bytes());
    out[12..16].copy_from_slice(&x[0].to_be_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);
    Ok(Some(Value::Int(16)))
}

pub(crate) fn register_bc_sm4_engine(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/crypto/engines/SM4Engine",
        "processBlock",
        "([BI[BI)I",
        bc_sm4_native_process_block,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn bc_xtea_read_sum(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<[u32; 32], MethodCallFailed> {
    let arr = match ctx.get_field_by_name(this, field) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "XTEA native: missing round-key state".into(),
            }
            .into())
        }
    };
    if ctx.array_length(arr) < 32 {
        return Err(RuntimeError::IllegalStateException {
            message: "XTEA native: malformed round-key state".into(),
        }
        .into());
    }
    let mut out = [0u32; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "XTEA native: malformed round-key state".into(),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub(crate) fn bc_xtea_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => 0,
    };

    if !matches!(ctx.get_field_by_name(this, "_initialised"), Value::Int(v) if v != 0) {
        return Err(RuntimeError::IllegalStateException {
            message: "XTEA not initialised".into(),
        }
        .into());
    }
    if in_off.saturating_add(8) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(8) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let sum0 = bc_xtea_read_sum(ctx, this, "_sum0")?;
    let sum1 = bc_xtea_read_sum(ctx, this, "_sum1")?;
    let mut block = [0u8; 8];
    ctx.read_byte_array_into(in_arr, in_off, &mut block);
    let mut v0 = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);

    if matches!(ctx.get_field_by_name(this, "_forEncryption"), Value::Int(v) if v != 0) {
        for i in 0..32 {
            v0 = v0.wrapping_add(((v1 << 4 ^ v1 >> 5).wrapping_add(v1)) ^ sum0[i]);
            v1 = v1.wrapping_add(((v0 << 4 ^ v0 >> 5).wrapping_add(v0)) ^ sum1[i]);
        }
    } else {
        for i in (0..32).rev() {
            v1 = v1.wrapping_sub(((v0 << 4 ^ v0 >> 5).wrapping_add(v0)) ^ sum1[i]);
            v0 = v0.wrapping_sub(((v1 << 4 ^ v1 >> 5).wrapping_add(v1)) ^ sum0[i]);
        }
    }

    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&v0.to_be_bytes());
    out[4..8].copy_from_slice(&v1.to_be_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);
    Ok(Some(Value::Int(8)))
}

pub(crate) fn register_bc_xtea_engine(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/crypto/engines/XTEAEngine",
        "processBlock",
        "([BI[BI)I",
        bc_xtea_native_process_block,
    );
    r.set_category(__prev_cat);
}

pub(crate) fn register_bc_gost3412_engine(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/crypto/engines/GOST3412_2015Engine",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
                _ => 0,
            };
            let out_arr = obj_arg(args, 3)?;
            let out_off = match args.get(4) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
                _ => 0,
            };

            let subkeys_outer = match ctx.get_field_by_name(this, "subKeys") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "GOST3412_2015 engine not initialised".into(),
                    }
                    .into())
                }
            };
            if in_off.saturating_add(16) > ctx.array_length(in_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too short",
                ));
            }
            if out_off.saturating_add(16) > ctx.array_length(out_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }
            let subkeys = bc_gost_read_subkeys(ctx, subkeys_outer).ok_or_else(|| {
                RuntimeError::IllegalStateException {
                    message: "GOST3412_2015 native: malformed subkey state".into(),
                }
            })?;
            let for_encryption =
                matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0);
            let mut input = [0u8; 16];
            ctx.read_byte_array_into(in_arr, in_off, &mut input);
            let output = bc_gost_process_block(&subkeys, for_encryption, &input);
            ctx.write_byte_array_from(out_arr, out_off, &output);
            Ok(Some(Value::Int(16)))
        },
    );
    r.set_category(__prev_cat);
}

/// Native fast-path for `org.bouncycastle.crypto.engines.AESEngine`'s private
/// single-block transforms. With `org/bouncycastle/*` JIT-banned, the
/// interpreted T-table AES dominates `AESTest`'s block-cipher Monte-Carlo
/// stress (the documented AES non-finish). These intercept the private
/// `encryptBlock`/`decryptBlock(byte[] in, int inOff, byte[] out, int outOff,
/// int[][] KW)` — which already receive the expanded key schedule and run with
/// all the public `processBlock` checks done — and apply a verbatim,
/// FIPS-197-validated port (see [`crate::bc_aes`]). A registered native fully
/// replaces the body (no decline-to-bytecode path), and `processBlock` has
/// already validated the buffers and non-null key, so the defensive
/// "can't happen" cases surface as an `IllegalStateException` rather than
/// silently no-op'ing the void method.
pub(crate) fn register_bc_aes_engine(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let aes = "org/bouncycastle/crypto/engines/AESEngine";
    let desc = "([BI[BI[[I)V";

    fn block_args(
        ctx: &dyn NativeContext,
        args: &[Value],
    ) -> Option<([u8; 16], usize, ObjectRef, usize, Vec<[u32; 4]>)> {
        let in_arr = obj_arg(args, 1).ok()?;
        let in_off = match args.get(2) {
            Some(Value::Int(v)) => *v as usize,
            _ => return None,
        };
        let out_arr = obj_arg(args, 3).ok()?;
        let out_off = match args.get(4) {
            Some(Value::Int(v)) => *v as usize,
            _ => return None,
        };
        let kw = read_aes_kw(ctx, obj_arg(args, 5).ok()?);
        if kw.len() < 2 {
            return None; // malformed schedule (can't happen post-init)
        }
        let mut inb = [0u8; 16];
        if ctx.read_byte_array_into(in_arr, in_off, &mut inb) != 16 {
            return None; // input shorter than a block (processBlock pre-checks this)
        }
        Some((inb, in_off, out_arr, out_off, kw))
    }

    fn bad_state() -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "AESEngine native: malformed block/key state".into(),
        }
        .into()
    }

    r.register(aes, "encryptBlock", desc, |ctx, args| {
        let (inb, _in_off, out_arr, out_off, kw) = block_args(ctx, args).ok_or_else(bad_state)?;
        let mut outb = [0u8; 16];
        crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
        ctx.write_byte_array_from(out_arr, out_off, &outb);
        Ok(None)
    });
    r.register(aes, "decryptBlock", desc, |ctx, args| {
        let (inb, _in_off, out_arr, out_off, kw) = block_args(ctx, args).ok_or_else(bad_state)?;
        let mut outb = [0u8; 16];
        crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
        ctx.write_byte_array_from(out_arr, out_off, &outb);
        Ok(None)
    });

    // KEEP (empty only while no constraints are installed): the key schedule
    // is built lazily by `init`/`generateWorkingKey` (registered below), not at
    // construction, and BouncyCastle's own `AESEngine()` declares no field
    // initialiser — so with the registrar at its default the native body is
    // genuinely nothing, which is what `newInstance()`'s `alloc_object` + the
    // real `MultiBlockCipher` call path want. The constructor is NOT empty
    // otherwise: it checks AES-256 against the installed constraints, so
    // `bc_aes_native_ctor` hands the call back to the bytecode when any are.
    r.register(aes, "<init>", "()V", bc_aes_native_ctor);
    r.register(
        aes,
        "newInstance",
        "()Lorg/bouncycastle/crypto/MultiBlockCipher;",
        |ctx, _args| {
            let class_id =
                ctx.ensure_class_initialized("org/bouncycastle/crypto/engines/AESEngine")?;
            let field_count = ctx.class_num_total_fields(class_id);
            let obj = ctx.alloc_object(class_id, field_count);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // generateWorkingKey(byte[] key, boolean forEncryption) -> int[][]. The
    // private key-schedule expansion; `AESTest.testCounter` churns it via
    // repeated `newCipher()`/`init`, so it became the hot frame once the block
    // transforms above went native. Also sets `this.ROUNDS` (the field's only
    // other readers are the now-native encrypt/decryptBlock).
    r.register(aes, "generateWorkingKey", "([BZ)[[I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_arr = obj_arg(args, 1)?;
        let for_enc = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);

        let klen = ctx.array_length(key_arr);
        let mut kbuf = [0u8; 32];
        let key: &[u8] = if klen <= 32 {
            let n = ctx.read_byte_array_into(key_arr, 0, &mut kbuf[..klen]);
            &kbuf[..n]
        } else {
            &[]
        };

        let w = match crate::bc_aes::generate_working_key(key, for_enc) {
            Some(w) => w,
            None => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Key length not 128/192/256 bits.".into(),
                }
                .into())
            }
        };

        // ROUNDS side effect (KC + 6 == rows - 1), matching BC.
        ctx.set_field_by_name(this, "ROUNDS", Value::Int((w.len() - 1) as i32));

        // Build the int[][] schedule. Holding `outer` across the inner
        // `new_array` calls is the same allocate-while-holding pattern as
        // `bi_alloc_int` — safe because GC here is stop-the-world-coordinated
        // and never fires mid-native-call.
        let outer = ctx.new_array(cratonvm_types::ArrayElementType::Reference, w.len());
        for (r_idx, cols) in w.iter().enumerate() {
            let row = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
            for (c, &word) in cols.iter().enumerate() {
                ctx.set_array_element(row, c, Value::Int(word as i32));
            }
            ctx.set_array_element(outer, r_idx, Value::Object(Some(row)));
        }
        Ok(Some(Value::Object(Some(outer))))
    });

    // init(boolean, CipherParameters). `AESTest.testCounter` constructs two
    // fresh AES-CTR engines per verify() call; with BC JIT-banned, the bytecode
    // init scaffolding remains hot even after block encryption and key schedule
    // generation are native.
    r.register(
        aes,
        "init",
        "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Same dropped `CryptoServicesRegistrar.checkConstraints` as in
            // `bc_aes_native_init`; see `bc_services_constraints_active`.
            if bc_services_constraints_active(ctx) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
                    &args[1..],
                );
            }
            let for_enc = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
            let params = obj_arg(args, 2)?;
            let key_arr = match ctx.get_field_by_name(params, "key") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "invalid parameter passed to AES init".into(),
                    }
                    .into())
                }
            };
            let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
            ctx.set_field_by_name(this, "WorkingKey", Value::Object(Some(wk)));
            ctx.set_field_by_name(this, "forEncryption", Value::Int(i32::from(for_enc)));
            if let Some(sbox) = bc_aes_clone_static_sbox(
                ctx,
                "org/bouncycastle/crypto/engines/AESEngine",
                if for_enc { "S" } else { "Si" },
            ) {
                ctx.set_field_by_name(this, "s", Value::Object(Some(sbox)));
            }
            Ok(None)
        },
    );

    r.register(
        aes,
        "processBlock",
        "([BI[BI)I",
        bc_aes_native_process_block,
    );

    for aes_impl in [
        "org/bouncycastle/crypto/engines/AESLightEngine",
        "org/bouncycastle/crypto/engines/AESFastEngine",
    ] {
        // Trivial constructor with one caveat. BouncyCastle's block ciphers
        // carry all their state in fields that `init(boolean,
        // CipherParameters)` writes — here `bc_aes_native_init` (registered a
        // few lines below for this same class) sets `ROUNDS`, `WorkingKey`,
        // `forEncryption` and `s`. The no-arg constructor declares no field
        // initializers and only chains to `Object.<init>`, and
        // `bc_aes_native_process_block` refuses to run on an object whose
        // `WorkingKey` is still unset ("AES engine not initialised"), so the
        // real initialiser is provably on the use path. What the body does
        // carry is a constraints check on `bitsOfSecurity()`, so
        // `bc_aes_native_ctor` runs the bytecode whenever constraints are
        // installed and stays empty otherwise. KEEP.
        r.register(aes_impl, "<init>", "()V", bc_aes_native_ctor);
        r.register(aes_impl, "encryptBlock", desc, bc_aes_native_encrypt_block);
        r.register(aes_impl, "decryptBlock", desc, bc_aes_native_decrypt_block);
        r.register(
            aes_impl,
            "generateWorkingKey",
            "([BZ)[[I",
            bc_aes_native_generate_working_key,
        );
        r.register(
            aes_impl,
            "init",
            "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
            bc_aes_native_init,
        );
        r.register(
            aes_impl,
            "processBlock",
            "([BI[BI)I",
            bc_aes_native_process_block,
        );
    }

    r.register(
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "reset",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let iv_arr = bc_sic_object_field(ctx, this, "IV")?;
            bc_sic_write_reset_counter(ctx, this, counter_arr, iv_arr)?;
            // `reset()` clears the advance-since-init accumulator and the sticky
            // out-of-range flag. Omitting this made the range bound un-clearable
            // rather than merely unenforced.
            bc_sic_reset_range(ctx, this);
            ctx.invoke_virtual(cipher, "reset", "()V", &[])?;
            Ok(None)
        },
    );

    r.register(
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "seekTo",
        "(J)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let position = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if position < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "position must be non-negative".into(),
                }
                .into());
            }
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let counter_out_arr = bc_sic_object_field(ctx, this, "counterOut")?;
            let iv_arr = bc_sic_object_field(ctx, this, "IV")?;
            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(bc_sic_bad_state("SICBlockCipher: zero block size"));
            }
            let mut counter = bc_sic_write_reset_counter(ctx, this, counter_arr, iv_arr)?;
            // Java `seekTo` is `reset()` then `skip(position)`, and `reset()`
            // clears the range state before the seek re-derives it.
            bc_sic_reset_range(ctx, this);
            let blocks = (position as u64) / (bs as u64);
            let byte_count = ((position as u64) % (bs as u64)) as i32;
            bc_sic_add_blocks(&mut counter, blocks);
            // `checkCounter`, not just the partial-IV prefix half of it — the
            // full-block-IV branch is the one that bounds the advance at 2^64
            // blocks and resyncs `used`.
            let iv_len = ctx.array_length(iv_arr);
            let mut iv = vec![0u8; iv_len];
            ctx.read_byte_array_into(iv_arr, 0, &mut iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta = vec![0u8; bs];
            let verdict = range.check_counter(&counter, &iv, &mut delta);
            range.write_back(ctx, this);
            verdict?;
            ctx.write_byte_array_from(counter_arr, 0, &counter);
            ctx.set_field_by_name(this, "byteCount", Value::Int(byte_count));
            bc_sic_encrypt_counter(ctx, cipher, counter_arr, counter_out_arr, &counter)?;
            Ok(Some(Value::Long(position)))
        },
    );

    r.register(
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "init",
        "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = obj_arg(args, 2)?;
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let iv_src = bc_sic_object_field(ctx, params, "iv")?;
            let iv_len = ctx.array_length(iv_src);
            let bs = ctx.array_length(counter_arr);
            if bs < iv_len {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("CTR/SIC mode requires IV no greater than: {bs} bytes."),
                }
                .into());
            }
            let max_counter_size = 8usize.min(bs / 2);
            if bs.saturating_sub(iv_len) > max_counter_size {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "CTR/SIC mode requires IV of at least: {} bytes.",
                        bs - max_counter_size
                    ),
                }
                .into());
            }

            if let Value::Object(Some(inner)) = ctx.get_field_by_name(params, "parameters") {
                ctx.invoke_virtual(
                    cipher,
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
                    &[Value::Int(1), Value::Object(Some(inner))],
                )?;
            }

            let iv_copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, iv_len);
            if iv_len != 0 {
                let mut iv = vec![0u8; iv_len];
                ctx.read_byte_array_into(iv_src, 0, &mut iv);
                ctx.write_byte_array_from(iv_copy, 0, &iv);
            }
            ctx.set_field_by_name(this, "IV", Value::Object(Some(iv_copy)));
            // The two derived fields the full-block-IV range bound is built on.
            // `fullBlockIV` read back FALSE for a 16-byte IV on a 16-byte block
            // because nothing here wrote it, which disarmed the `used`
            // wrap-around detection for every block size of 8 or less.
            let full_block_iv = iv_len == bs;
            ctx.set_field_by_name(this, "fullBlockIV", Value::Int(i32::from(full_block_iv)));
            let lane_off = match ctx.get_field_by_name(this, "laneOff") {
                Value::Int(v) => v.max(0) as usize,
                _ => 0,
            };
            if full_block_iv && lane_off > 0 {
                let guard = match ctx.get_array_element(iv_copy, lane_off - 1) {
                    Value::Int(v) => (v as u8).wrapping_add(1),
                    _ => 0,
                };
                ctx.set_field_by_name(this, "guardByte", Value::Int(guard as i8 as i32));
            }
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            bc_sic_write_reset_counter(ctx, this, counter_arr, iv_copy)?;
            bc_sic_reset_range(ctx, this);
            ctx.invoke_virtual(cipher, "reset", "()V", &[])?;
            Ok(None)
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_stream_read_i32x16(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i32; 16]> {
    if ctx.array_length(arr) != 16 {
        return None;
    }
    let mut out = [0i32; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v,
            _ => return None,
        }
    }
    Some(out)
}

pub(crate) fn bc_stream_write_i32x16(ctx: &dyn NativeContext, arr: ObjectRef, words: &[i32; 16]) {
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(word));
    }
}

pub(crate) fn bc_stream_words_to_le_bytes(words: &[i32; 16]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&(words[i] as u32).to_le_bytes());
    }
    out
}

pub(crate) fn bc_stream_generate_key_stream(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    rounds: i32,
    engine_state: &[i32; 16],
    x_arr: ObjectRef,
    key_stream_arr: ObjectRef,
    key_stream: &mut [u8; 64],
) -> Result<(), MethodCallFailed> {
    let mut x = [0i32; 16];
    match class_name {
        "org/bouncycastle/crypto/engines/ChaChaEngine"
        | "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            crate::bc_chacha::chacha_core(rounds, engine_state, &mut x);
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine"
        | "org/bouncycastle/crypto/engines/XSalsa20Engine" => {
            crate::bc_chacha::salsa_core(rounds, engine_state, &mut x);
        }
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: unsupported engine class".into(),
            }
            .into())
        }
    }
    bc_stream_write_i32x16(ctx, x_arr, &x);
    *key_stream = bc_stream_words_to_le_bytes(&x);
    ctx.write_byte_array_from(key_stream_arr, 0, key_stream);
    Ok(())
}

pub(crate) fn bc_stream_advance_counter(
    class_name: &str,
    engine_state: &mut [i32; 16],
) -> Result<(), MethodCallFailed> {
    match class_name {
        "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            engine_state[12] = engine_state[12].wrapping_add(1);
            if engine_state[12] == 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "attempt to increase counter past 2^32.".into(),
                }
                .into());
            }
        }
        "org/bouncycastle/crypto/engines/ChaChaEngine" => {
            engine_state[12] = engine_state[12].wrapping_add(1);
            if engine_state[12] == 0 {
                engine_state[13] = engine_state[13].wrapping_add(1);
            }
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine"
        | "org/bouncycastle/crypto/engines/XSalsa20Engine" => {
            engine_state[8] = engine_state[8].wrapping_add(1);
            if engine_state[8] == 0 {
                engine_state[9] = engine_state[9].wrapping_add(1);
            }
        }
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: unsupported counter layout".into(),
            }
            .into())
        }
    }
    Ok(())
}

pub(crate) fn bc_salsa20_process_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let out_arr = obj_arg(args, 4)?;
    let out_off_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if !matches!(ctx.get_field_by_name(this, "initialised"), Value::Int(v) if v != 0) {
        return Err(RuntimeError::IllegalStateException {
            message: "Salsa20Engine not initialised".into(),
        }
        .into());
    }
    if in_off_i < 0
        || len_i < 0
        || out_off_i < 0
        || (in_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(in_arr)
    {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if (out_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    // SECURITY — do NOT restore the `_ => 20` default here.
    //
    // `rounds` is the only thing that makes the ChaCha/Salsa permutation a
    // permutation: `chacha_core(0, ..)` / `salsa_core(0, ..)` is the identity,
    // so the "keystream" is the engine state and the key is recoverable
    // straight out of the ciphertext (see `check_rounds` in
    // `native-builtins-crypto/src/bc_chacha.rs`, which names this call site as
    // one of the two arms that validate nothing).
    //
    // The previous read was `match ctx.get_field_by_name(this, "rounds") {
    // Value::Int(v) => v, _ => 20 }`. That looks like it defaults to 20, but it
    // does not: `get_field_by_name` is not descriptor-aware, so an unwritten
    // slot answers `Value::Int(0)` — which the FIRST arm accepts. The `_ => 20`
    // fallback only ever fires when the field does not resolve at all. So the
    // dangerous input (0) took the "valid value" path and the safe default was
    // unreachable for it. `int_field_strict` reads by resolved index instead
    // (descriptor-decoded) and returns `None` for an absent field, and we
    // refuse outright rather than guess: a round count we cannot read is not a
    // round count we may substitute.
    let rounds = match crate::field_read::int_field_strict(ctx, this, "rounds") {
        Some(v) if v > 0 && v % 2 == 0 => v,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: refusing to generate a keystream with an \
                          unreadable or illegal round count"
                    .into(),
            }
            .into())
        }
    };
    let engine_state_arr = match ctx.get_field_by_name(this, "engineState") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing engineState".into(),
            }
            .into())
        }
    };
    let x_arr = match ctx.get_field_by_name(this, "x") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing x".into(),
            }
            .into())
        }
    };
    let key_stream_arr = match ctx.get_field_by_name(this, "keyStream") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing keyStream".into(),
            }
            .into())
        }
    };
    let mut index = match ctx.get_field_by_name(this, "index") {
        Value::Int(v) => v as usize,
        _ => 0,
    } & 63;
    let mut engine_state = bc_stream_read_i32x16(ctx, engine_state_arr).ok_or_else(|| {
        RuntimeError::IllegalStateException {
            message: "Salsa20Engine.processBytes: malformed engineState".into(),
        }
    })?;
    let mut key_stream = [0u8; 64];
    ctx.read_byte_array_into(key_stream_arr, 0, &mut key_stream);

    let len = len_i as usize;
    let mut input = vec![0u8; len];
    ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
    let mut output = vec![0u8; len];
    let mut pos = 0usize;
    while pos < len {
        let take = (64 - index).min(len - pos);
        for i in 0..take {
            output[pos + i] = input[pos + i] ^ key_stream[index + i];
        }
        pos += take;
        index = (index + take) & 63;
        if index == 0 {
            bc_stream_advance_counter(&class_name, &mut engine_state)?;
            bc_stream_generate_key_stream(
                ctx,
                &class_name,
                rounds,
                &engine_state,
                x_arr,
                key_stream_arr,
                &mut key_stream,
            )?;
        }
    }

    bc_stream_write_i32x16(ctx, engine_state_arr, &engine_state);
    ctx.set_field_by_name(this, "index", Value::Int(index as i32));
    ctx.write_byte_array_from(out_arr, out_off_i as usize, &output);
    Ok(Some(Value::Int(len_i)))
}

pub(crate) fn bc_vmpc_process_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let out_arr = obj_arg(args, 4)?;
    let out_off_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if in_off_i < 0
        || len_i < 0
        || out_off_i < 0
        || (in_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(in_arr)
    {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if (out_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let p_arr = match ctx.get_field_by_name(this, "P") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "VMPCEngine not initialised".into(),
            }
            .into())
        }
    };
    if ctx.array_length(p_arr) != 256 {
        return Err(RuntimeError::IllegalStateException {
            message: "VMPCEngine: malformed P".into(),
        }
        .into());
    }
    let mut p = vec![0u8; 256];
    ctx.read_byte_array_into(p_arr, 0, &mut p);
    let mut s = match ctx.get_field_by_name(this, "s") {
        Value::Int(v) => v as u8,
        _ => 0,
    };
    let mut n = match ctx.get_field_by_name(this, "n") {
        Value::Int(v) => v as u8,
        _ => 0,
    };
    let len = len_i as usize;
    let mut input = vec![0u8; len];
    ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
    let mut output = vec![0u8; len];

    for i in 0..len {
        let n_idx = n as usize;
        s = p[s.wrapping_add(p[n_idx]) as usize];
        let s_idx = s as usize;
        let z_idx = p[p[s_idx] as usize].wrapping_add(1) as usize;
        let z = p[z_idx];
        p.swap(n_idx, s_idx);
        n = n.wrapping_add(1);
        output[i] = input[i] ^ z;
    }

    ctx.write_byte_array_from(p_arr, 0, &p);
    ctx.set_field_by_name(this, "s", Value::Int((s as i8) as i32));
    ctx.set_field_by_name(this, "n", Value::Int((n as i8) as i32));
    ctx.write_byte_array_from(out_arr, out_off_i as usize, &output);
    Ok(Some(Value::Int(len_i)))
}

/// Native fast-path for the BouncyCastle ChaCha permutation kernels that
/// dominate SPHINCS-256 (`org.bouncycastle.pqc.crypto.test.RegressionTest`,
/// which otherwise times out >360 s vs HotSpot ~1.3 s). Intercepts the two
/// `public static` pure ChaCha cores:
///   * `ChaChaEngine.chachaCore(int rounds, int[] input, int[] x)` — the PRG
///     block function (`Seed.prg` → `Salsa20Engine.processBytes` →
///     `generateKeyStream`), the profiled hot leaf.
///   * `Permute.permute(int rounds, int[] x)` — the SPHINCS hash permutation
///     (`HashFunctions.hash_2n_n`/`hash_n_n`).
/// With `org/bouncycastle/*` JIT-banned, both run interpreted and their dozens
/// of per-block `Integers.rotateLeft` *method calls* crush the interpreter. The
/// Rust bodies (`crate::bc_chacha`) are verbatim, RFC 8439-validated ports.
/// Both are invoked via invokestatic, so the native registry shadows the
/// bytecode (`execute_invokestatic` `direct_native`). Length/odd-rounds guards
/// mirror BC's `IllegalArgumentException`s (defensive — never fire in the real
/// callers, which always pass length-16 arrays and even rounds).
pub(crate) fn register_bc_chacha(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    // Read a Java int[16] into a Rust array; None when the length isn't 16
    // (→ caller raises IAE, matching BC's `x.length != 16` guard).
    fn read16(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i32; 16]> {
        if ctx.array_length(arr) != 16 {
            return None;
        }
        let mut out = [0i32; 16];
        for k in 0..16 {
            match ctx.get_array_element(arr, k) {
                Value::Int(v) => out[k] = v,
                _ => return None,
            }
        }
        Some(out)
    }

    fn iae(msg: &str) -> MethodCallFailed {
        RuntimeError::IllegalArgumentException {
            message: msg.into(),
        }
        .into()
    }

    for cls in [
        "org/bouncycastle/crypto/engines/Salsa20Engine",
        "org/bouncycastle/crypto/engines/XSalsa20Engine",
        "org/bouncycastle/crypto/engines/ChaChaEngine",
        "org/bouncycastle/crypto/engines/ChaCha7539Engine",
        "org/bouncycastle/crypto/engines/XChaCha20Engine",
    ] {
        r.register(cls, "processBytes", "([BII[BI)I", bc_salsa20_process_bytes);
    }

    for cls in [
        "org/bouncycastle/crypto/engines/VMPCEngine",
        "org/bouncycastle/crypto/engines/VMPCKSA3Engine",
    ] {
        r.register(cls, "processBytes", "([BII[BI)I", bc_vmpc_process_bytes);
    }

    // ChaChaEngine.chachaCore: x[i] = permute(input)_i + input[i]. `input` and
    // `x` are distinct arrays (engine state vs keystream buffer).
    r.register(
        "org/bouncycastle/crypto/engines/ChaChaEngine",
        "chachaCore",
        "(I[I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("chachaCore: missing rounds")),
            };
            let input_arr = obj_arg(args, 1)?;
            let x_arr = obj_arg(args, 2)?;
            // BC checks input.length, then x.length, then rounds parity.
            let input = read16(ctx, input_arr).ok_or_else(|| iae(""))?;
            if ctx.array_length(x_arr) != 16 {
                return Err(iae(""));
            }
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            let mut x = [0i32; 16];
            crate::bc_chacha::chacha_core(rounds, &input, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    r.register(
        "org/bouncycastle/crypto/engines/Salsa20Engine",
        "salsaCore",
        "(I[I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("salsaCore: missing rounds")),
            };
            let input_arr = obj_arg(args, 1)?;
            let x_arr = obj_arg(args, 2)?;
            let input = read16(ctx, input_arr).ok_or_else(|| iae(""))?;
            if ctx.array_length(x_arr) != 16 {
                return Err(iae(""));
            }
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            let mut x = [0i32; 16];
            crate::bc_chacha::salsa_core(rounds, &input, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    // Permute.permute: in-place permutation of x, no final input-add.
    r.register(
        "org/bouncycastle/pqc/crypto/sphincs/Permute",
        "permute",
        "(I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("permute: missing rounds")),
            };
            let x_arr = obj_arg(args, 1)?;
            let mut x = read16(ctx, x_arr).ok_or_else(|| iae(""))?;
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            crate::bc_chacha::permute(rounds, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    // Permute.chacha_permute(byte[] out, byte[] in): the SPHINCS hash leaf
    // (HashFunctions.hash_n_n/hash_2n_n), the dominant frame in tree/WOTS
    // signing. Folds in the per-call int[16] allocation + 32 Pack conversions
    // that the bytecode wraps around `permute`. Instance method (invokevirtual,
    // dispatched via the native-override check in execute_invokevirtual_cached):
    // args = [this (Permute), out, in]. `in` and `out` alias in the callers
    // (`chacha_permute(x, x)`); we read `in` fully before writing `out`.
    r.register(
        "org/bouncycastle/pqc/crypto/sphincs/Permute",
        "chacha_permute",
        "([B[B)V",
        |ctx, args| {
            let out_arr = obj_arg(args, 1)?;
            let in_arr = obj_arg(args, 2)?;
            // The bytecode reads in[0..64) and writes out[0..64); a buffer
            // shorter than 64 would AIOOBE in Pack — mirror that.
            let in_len = ctx.array_length(in_arr);
            let out_len = ctx.array_length(out_arr);
            if in_len < 64 || out_len < 64 {
                let index = if in_len < 64 { in_len } else { out_len } as i32;
                return Err(RuntimeError::aioobe_index_only(index).into());
            }
            let mut inb = [0u8; 64];
            if ctx.read_byte_array_into(in_arr, 0, &mut inb) != 64 {
                return Err(RuntimeError::aioobe_index_only(0).into());
            }
            let mut outb = [0u8; 64];
            crate::bc_chacha::chacha_permute_bytes(&mut outb, &inb);
            ctx.write_byte_array_from(out_arr, 0, &outb);
            Ok(None)
        },
    );

    // SPHINCS hash layer (HashFunctions instance methods, invokevirtual). Once
    // chacha_permute is native, the per-hash byte[64]/byte[32] allocations +
    // copy/XOR loops in hash_n_n/hash_2n_n dominate tree/WOTS signing (millions
    // of calls). Folding them native eliminates that glue. All return int 0
    // (BC). The byte-level logic lives in `crate::bc_chacha` (HotSpot-validated).
    fn ioff(args: &[Value], i: usize) -> usize {
        match args.get(i) {
            Some(Value::Int(v)) => *v as usize,
            _ => 0,
        }
    }
    let hf = "org/bouncycastle/pqc/crypto/sphincs/HashFunctions";

    r.register(hf, "hash_n_n", "([BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mut in32 = [0u8; 32];
        if ctx.read_byte_array_into(inp, in_off, &mut in32) != 32 {
            return Err(iae(""));
        }
        let res = crate::bc_chacha::sphincs_hash_n_n(&in32);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    r.register(hf, "hash_2n_n", "([BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mut in64 = [0u8; 64];
        if ctx.read_byte_array_into(inp, in_off, &mut in64) != 64 {
            return Err(iae(""));
        }
        let res = crate::bc_chacha::sphincs_hash_2n_n(&in64);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    r.register(hf, "hash_n_n_mask", "([BI[BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mask = obj_arg(args, 5)?;
        let mask_off = ioff(args, 6);
        let mut in32 = [0u8; 32];
        let mut m32 = [0u8; 32];
        if ctx.read_byte_array_into(inp, in_off, &mut in32) != 32
            || ctx.read_byte_array_into(mask, mask_off, &mut m32) != 32
        {
            return Err(iae(""));
        }
        for i in 0..32 {
            in32[i] ^= m32[i];
        }
        let res = crate::bc_chacha::sphincs_hash_n_n(&in32);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    r.register(hf, "hash_2n_n_mask", "([BI[BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mask = obj_arg(args, 5)?;
        let mask_off = ioff(args, 6);
        let mut in64 = [0u8; 64];
        let mut m64 = [0u8; 64];
        if ctx.read_byte_array_into(inp, in_off, &mut in64) != 64
            || ctx.read_byte_array_into(mask, mask_off, &mut m64) != 64
        {
            return Err(iae(""));
        }
        for i in 0..64 {
            in64[i] ^= m64[i];
        }
        let res = crate::bc_chacha::sphincs_hash_2n_n(&in64);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    r.set_category(__prev_cat);
}

/// Native fast-path for the BouncyCastle NewHope (post-quantum) lattice kernels
/// that dominate `NewHopeTest` (1000 key-exchange rounds) once the ChaCha cores
/// are native: the number-theoretic transform `Poly.toNTT`/`fromNTT` (the
/// profiled hot frame — interpreted `short[]` Montgomery butterflies) and the
/// SHAKE128 rejection sampler `Poly.uniform` (interpreted Keccak). All are
/// `public static` over `short[1024]`; the bodies in `crate::bc_newhope` are
/// verbatim ports validated element-for-element against HotSpot. Invoked via
/// invokestatic (registry-shadowed). `short[]` reads/writes go through
/// `get/set_array_element` (sign-extended `Value::Int` <-> i16).
pub(crate) fn register_bc_newhope(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    fn read_poly(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i16; 1024]> {
        if ctx.array_length(arr) != 1024 {
            return None;
        }
        let mut out = [0i16; 1024];
        for (k, slot) in out.iter_mut().enumerate() {
            match ctx.get_array_element(arr, k) {
                Value::Int(v) => *slot = v as i16,
                _ => return None,
            }
        }
        Some(out)
    }
    fn write_poly(ctx: &dyn NativeContext, arr: ObjectRef, vals: &[i16; 1024]) {
        for (k, &v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, k, Value::Int(v as i32));
        }
    }
    fn bad() -> MethodCallFailed {
        RuntimeError::aioobe_index_only(1024).into()
    }

    let poly = "org/bouncycastle/pqc/crypto/newhope/Poly";

    r.register(poly, "toNTT", "([S)V", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let mut r = read_poly(ctx, arr).ok_or_else(bad)?;
        crate::bc_newhope::to_ntt(&mut r);
        write_poly(ctx, arr, &r);
        Ok(None)
    });

    r.register(poly, "fromNTT", "([S)V", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let mut r = read_poly(ctx, arr).ok_or_else(bad)?;
        crate::bc_newhope::from_ntt(&mut r);
        write_poly(ctx, arr, &r);
        Ok(None)
    });

    r.register(poly, "uniform", "([S[B)V", |ctx, args| {
        let a_arr = obj_arg(args, 0)?;
        if ctx.array_length(a_arr) != 1024 {
            return Err(bad());
        }
        let seed_arr = obj_arg(args, 1)?;
        let slen = ctx.array_length(seed_arr);
        let mut seed = vec![0u8; slen];
        ctx.read_byte_array_into(seed_arr, 0, &mut seed);
        let mut a = [0i16; 1024];
        crate::bc_newhope::uniform(&mut a, &seed);
        write_poly(ctx, a_arr, &a);
        Ok(None)
    });

    r.set_category(__prev_cat);
}

pub(crate) fn register_bc_cbc_block_cipher(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        "org/bouncycastle/crypto/modes/CBCBlockCipher",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off_i = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let out_arr = obj_arg(args, 3)?;
            let out_off_i = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            if in_off_i < 0 {
                return Err(RuntimeError::aioobe_index_only(in_off_i).into());
            }
            if out_off_i < 0 {
                return Err(RuntimeError::aioobe_index_only(out_off_i).into());
            }
            let in_off = in_off_i as usize;
            let out_off = out_off_i as usize;
            let block_size = match ctx.get_field_by_name(this, "blockSize") {
                Value::Int(v) if v > 0 => v as usize,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CBCBlockCipher: malformed block size".into(),
                    }
                    .into())
                }
            };
            let encrypting =
                matches!(ctx.get_field_by_name(this, "encrypting"), Value::Int(v) if v != 0);
            let cipher = match ctx.get_field_by_name(this, "cipher") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CBCBlockCipher not initialised".into(),
                    }
                    .into())
                }
            };
            let cbc_v_arr = match ctx.get_field_by_name(this, "cbcV") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CBCBlockCipher: missing cbcV".into(),
                    }
                    .into())
                }
            };
            let cbc_next_v_arr = match ctx.get_field_by_name(this, "cbcNextV") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "CBCBlockCipher: missing cbcNextV".into(),
                    }
                    .into())
                }
            };

            if in_off.saturating_add(block_size) > ctx.array_length(in_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too short",
                ));
            }

            let is_aes = ctx
                .class_name_arc_of_id(ctx.class_id_of_object(cipher))
                .as_deref()
                == Some("org/bouncycastle/crypto/engines/AESEngine");
            let kw = if is_aes {
                match ctx.get_field_by_name(cipher, "WorkingKey") {
                    Value::Object(Some(wk)) => read_aes_kw(ctx, wk),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let use_aes = block_size == 16 && kw.len() >= 2;

            if encrypting {
                let mut cbc_v = vec![0u8; block_size];
                ctx.read_byte_array_into(cbc_v_arr, 0, &mut cbc_v);
                let mut input = vec![0u8; block_size];
                ctx.read_byte_array_into(in_arr, in_off, &mut input);
                for i in 0..block_size {
                    cbc_v[i] ^= input[i];
                }

                if use_aes {
                    if out_off.saturating_add(block_size) > ctx.array_length(out_arr) {
                        return Err(bc_gost_throw_crypto_exception(
                            ctx,
                            "org/bouncycastle/crypto/OutputLengthException",
                            "output buffer too short",
                        ));
                    }
                    let mut block = [0u8; 16];
                    block.copy_from_slice(&cbc_v[..16]);
                    let mut out_block = [0u8; 16];
                    crate::bc_aes::encrypt_block(&kw, &block, &mut out_block);
                    ctx.write_byte_array_from(out_arr, out_off, &out_block);
                    ctx.write_byte_array_from(cbc_v_arr, 0, &out_block);
                    return Ok(Some(Value::Int(16)));
                }

                ctx.write_byte_array_from(cbc_v_arr, 0, &cbc_v);
                let result = ctx.invoke_virtual(
                    cipher,
                    "processBlock",
                    "([BI[BI)I",
                    &[
                        Value::Object(Some(cbc_v_arr)),
                        Value::Int(0),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                )?;
                let mut out_block = vec![0u8; block_size];
                ctx.read_byte_array_into(out_arr, out_off, &mut out_block);
                ctx.write_byte_array_from(cbc_v_arr, 0, &out_block);
                return Ok(result.or(Some(Value::Int(block_size as i32))));
            }

            if out_off.saturating_add(block_size) > ctx.array_length(out_arr) && use_aes {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }

            let mut input = vec![0u8; block_size];
            ctx.read_byte_array_into(in_arr, in_off, &mut input);
            ctx.write_byte_array_from(cbc_next_v_arr, 0, &input);

            let result = if use_aes {
                let mut block = [0u8; 16];
                block.copy_from_slice(&input[..16]);
                let mut out_block = [0u8; 16];
                crate::bc_aes::decrypt_block(&kw, &block, &mut out_block);
                ctx.write_byte_array_from(out_arr, out_off, &out_block);
                Some(Value::Int(16))
            } else {
                ctx.invoke_virtual(
                    cipher,
                    "processBlock",
                    "([BI[BI)I",
                    &[
                        Value::Object(Some(in_arr)),
                        Value::Int(in_off_i),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                )?
            };

            let mut cbc_v = vec![0u8; block_size];
            ctx.read_byte_array_into(cbc_v_arr, 0, &mut cbc_v);
            let mut out_block = vec![0u8; block_size];
            ctx.read_byte_array_into(out_arr, out_off, &mut out_block);
            for i in 0..block_size {
                out_block[i] ^= cbc_v[i];
            }
            ctx.write_byte_array_from(out_arr, out_off, &out_block);
            ctx.set_field_by_name(this, "cbcV", Value::Object(Some(cbc_next_v_arr)));
            ctx.set_field_by_name(this, "cbcNextV", Value::Object(Some(cbc_v_arr)));

            Ok(result.or(Some(Value::Int(block_size as i32))))
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_pack_offset_arg(args: &[Value], idx: usize) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Err(RuntimeError::aioobe_index_only(-1).into()),
    }
}

pub(crate) fn bc_pack_int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

pub(crate) fn bc_pack_check_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Result<(), MethodCallFailed> {
    let end = match off.checked_add(len) {
        Some(v) => v,
        None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
    };
    if ctx.array_length(arr) < end {
        return Err(RuntimeError::aioobe_index_only(end as i32).into());
    }
    Ok(())
}

pub(crate) fn bc_pack_read_be_i32(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
) -> Result<i32, MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    let mut bytes = [0u8; 4];
    ctx.read_byte_array_into(arr, off, &mut bytes);
    Ok(i32::from_be_bytes(bytes))
}

pub(crate) fn bc_pack_read_le_i32(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
) -> Result<i32, MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    let mut bytes = [0u8; 4];
    ctx.read_byte_array_into(arr, off, &mut bytes);
    Ok(i32::from_le_bytes(bytes))
}

pub(crate) fn bc_pack_write_be_i32(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    value: i32,
) -> Result<(), MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    ctx.write_byte_array_from(arr, off, &value.to_be_bytes());
    Ok(())
}

pub(crate) fn bc_pack_write_le_i32(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    value: i32,
) -> Result<(), MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    ctx.write_byte_array_from(arr, off, &value.to_le_bytes());
    Ok(())
}

pub(crate) fn bc_pack_check_int_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Result<(), MethodCallFailed> {
    let end = match off.checked_add(len) {
        Some(v) => v,
        None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
    };
    if ctx.array_length(arr) < end {
        return Err(RuntimeError::aioobe_index_only(end as i32).into());
    }
    Ok(())
}

pub(crate) fn register_bc_pack_helpers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let pack = "org/bouncycastle/util/Pack";

    r.register(pack, "bigEndianToInt", "([BI)I", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let off = bc_pack_offset_arg(args, 1)?;
        Ok(Some(Value::Int(bc_pack_read_be_i32(ctx, bs, off)?)))
    });
    r.register(pack, "littleEndianToInt", "([BI)I", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let off = bc_pack_offset_arg(args, 1)?;
        Ok(Some(Value::Int(bc_pack_read_le_i32(ctx, bs, off)?)))
    });

    r.register(pack, "bigEndianToInt", "([BI[I)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let word = bc_pack_read_be_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });
    r.register(pack, "littleEndianToInt", "([BI[I)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let word = bc_pack_read_le_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });

    r.register(pack, "bigEndianToInt", "([BI[III)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_off = bc_pack_offset_arg(args, 3)?;
        let count = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let word = bc_pack_read_be_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, n_off + i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });
    r.register(pack, "littleEndianToInt", "([BI[III)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_off = bc_pack_offset_arg(args, 3)?;
        let count = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let word = bc_pack_read_le_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, n_off + i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });

    r.register(pack, "intToBigEndian", "(I[BI)V", |ctx, args| {
        let value = bc_pack_int_arg(args, 0);
        let bs = obj_arg(args, 1)?;
        let off = bc_pack_offset_arg(args, 2)?;
        bc_pack_write_be_i32(ctx, bs, off, value)?;
        Ok(None)
    });
    r.register(pack, "intToLittleEndian", "(I[BI)V", |ctx, args| {
        let value = bc_pack_int_arg(args, 0);
        let bs = obj_arg(args, 1)?;
        let off = bc_pack_offset_arg(args, 2)?;
        bc_pack_write_le_i32(ctx, bs, off, value)?;
        Ok(None)
    });

    r.register(pack, "intToBigEndian", "([I[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let bs = obj_arg(args, 1)?;
        let mut b_off = bc_pack_offset_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let value = match ctx.get_array_element(ns, i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_be_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });
    r.register(pack, "intToLittleEndian", "([I[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let bs = obj_arg(args, 1)?;
        let mut b_off = bc_pack_offset_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let value = match ctx.get_array_element(ns, i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_le_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });

    r.register(pack, "intToBigEndian", "([III[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let n_off = bc_pack_offset_arg(args, 1)?;
        let count = bc_pack_offset_arg(args, 2)?;
        let bs = obj_arg(args, 3)?;
        let mut b_off = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let value = match ctx.get_array_element(ns, n_off + i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_be_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });
    r.register(pack, "intToLittleEndian", "([III[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let n_off = bc_pack_offset_arg(args, 1)?;
        let count = bc_pack_offset_arg(args, 2)?;
        let bs = obj_arg(args, 3)?;
        let mut b_off = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let value = match ctx.get_array_element(ns, n_off + i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_le_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });

    r.set_category(__prev_cat);
}

pub(crate) const BC_X25519_M25: i32 = 0x01ff_ffff;

pub(crate) const BC_X25519_M26: i32 = 0x03ff_ffff;

pub(crate) fn bc_x25519_read_limb_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
) -> Result<[i32; 10], MethodCallFailed> {
    bc_pack_check_int_array(ctx, arr, 0, 10)?;
    let mut out = [0i32; 10];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "X25519Field native: malformed limb array".into(),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub(crate) fn bc_x25519_write_limb_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    words: &[i32; 10],
) {
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(word));
    }
}

pub(crate) fn bc_x25519_field_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_x25519_read_limb_array(ctx, x_arr)?;
    let y = bc_x25519_read_limb_array(ctx, y_arr)?;
    bc_pack_check_int_array(ctx, z_arr, 0, 10)?;

    let mut x0 = x[0] as i64;
    let mut y0 = y[0] as i64;
    let mut x1 = x[1] as i64;
    let mut y1 = y[1] as i64;
    let mut x2 = x[2] as i64;
    let mut y2 = y[2] as i64;
    let mut x3 = x[3] as i64;
    let mut y3 = y[3] as i64;
    let mut x4 = x[4] as i64;
    let mut y4 = y[4] as i64;

    let u0 = x[5] as i64;
    let v0 = y[5] as i64;
    let u1 = x[6] as i64;
    let v1 = y[6] as i64;
    let u2 = x[7] as i64;
    let v2 = y[7] as i64;
    let u3 = x[8] as i64;
    let v3 = y[8] as i64;
    let u4 = x[9] as i64;
    let v4 = y[9] as i64;

    let mut a0 = x0 * y0;
    let mut a1 = x0 * y1 + x1 * y0;
    let mut a2 = x0 * y2 + x1 * y1 + x2 * y0;
    let mut a3 = (x1 * y2 + x2 * y1) << 1;
    a3 += x0 * y3 + x3 * y0;
    let mut a4 = (x2 * y2) << 1;
    a4 += x0 * y4 + x1 * y3 + x3 * y1 + x4 * y0;
    let mut a5 = (x1 * y4 + x2 * y3 + x3 * y2 + x4 * y1) << 1;
    let mut a6 = (x2 * y4 + x4 * y2) << 1;
    a6 += x3 * y3;
    let mut a7 = x3 * y4 + x4 * y3;
    let mut a8 = (x4 * y4) << 1;

    let b0 = u0 * v0;
    let b1 = u0 * v1 + u1 * v0;
    let b2 = u0 * v2 + u1 * v1 + u2 * v0;
    let mut b3 = (u1 * v2 + u2 * v1) << 1;
    b3 += u0 * v3 + u3 * v0;
    let mut b4 = (u2 * v2) << 1;
    b4 += u0 * v4 + u1 * v3 + u3 * v1 + u4 * v0;
    let b5 = u1 * v4 + u2 * v3 + u3 * v2 + u4 * v1;
    let mut b6 = (u2 * v4 + u4 * v2) << 1;
    b6 += u3 * v3;
    let b7 = u3 * v4 + u4 * v3;
    let b8 = u4 * v4;

    a0 -= b5 * 76;
    a1 -= b6 * 38;
    a2 -= b7 * 38;
    a3 -= b8 * 76;

    a5 -= b0;
    a6 -= b1;
    a7 -= b2;
    a8 -= b3;

    x0 += u0;
    y0 += v0;
    x1 += u1;
    y1 += v1;
    x2 += u2;
    y2 += v2;
    x3 += u3;
    y3 += v3;
    x4 += u4;
    y4 += v4;

    let c0 = x0 * y0;
    let c1 = x0 * y1 + x1 * y0;
    let c2 = x0 * y2 + x1 * y1 + x2 * y0;
    let mut c3 = (x1 * y2 + x2 * y1) << 1;
    c3 += x0 * y3 + x3 * y0;
    let mut c4 = (x2 * y2) << 1;
    c4 += x0 * y4 + x1 * y3 + x3 * y1 + x4 * y0;
    let c5 = (x1 * y4 + x2 * y3 + x3 * y2 + x4 * y1) << 1;
    let mut c6 = (x2 * y4 + x4 * y2) << 1;
    c6 += x3 * y3;
    let c7 = x3 * y4 + x4 * y3;
    let c8 = (x4 * y4) << 1;

    let mut z = [0i32; 10];
    let mut t = a8 + (c3 - a3);
    let z8 = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += (c4 - a4) - b4;
    let z9 = (t as i32) & BC_X25519_M25;
    t >>= 25;
    t = a0 + (t + c5 - a5) * 38;
    z[0] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += a1 + (c6 - a6) * 38;
    z[1] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += a2 + (c7 - a7) * 38;
    z[2] = (t as i32) & BC_X25519_M25;
    t >>= 25;
    t += a3 + (c8 - a8) * 38;
    z[3] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += a4 + b4 * 38;
    z[4] = (t as i32) & BC_X25519_M25;
    t >>= 25;
    t += a5 + (c0 - a0);
    z[5] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += a6 + (c1 - a1);
    z[6] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    t += a7 + (c2 - a2);
    z[7] = (t as i32) & BC_X25519_M25;
    t >>= 25;
    t += z8 as i64;
    z[8] = (t as i32) & BC_X25519_M26;
    t >>= 26;
    z[9] = z9 + t as i32;

    bc_x25519_write_limb_array(ctx, z_arr, &z);
    Ok(None)
}

pub(crate) fn register_bc_x25519_field(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/math/ec/rfc7748/X25519Field",
        "mul",
        "([I[I[I)V",
        bc_x25519_field_mul,
    );
    r.set_category(__prev_cat);
}

pub(crate) const BC_X448_SIZE: usize = 16;

pub(crate) const BC_X448_M28: u32 = 0x0fff_ffff;

pub(crate) fn bc_x448_modulus() -> &'static num_bigint::BigInt {
    use std::sync::OnceLock;

    static MODULUS: OnceLock<num_bigint::BigInt> = OnceLock::new();
    MODULUS.get_or_init(|| {
        let one = num_bigint::BigUint::from(1u32);
        let modulus =
            (one.clone() << 448usize) - (one.clone() << 224usize) - num_bigint::BigUint::from(1u32);
        num_bigint::BigInt::from_biguint(num_bigint::Sign::Plus, modulus)
    })
}

pub(crate) fn bc_x448_read_field_value(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
) -> Result<num_bigint::BigInt, MethodCallFailed> {
    bc_pack_check_int_array(ctx, arr, 0, BC_X448_SIZE)?;
    let mut value = num_bigint::BigInt::from(0i32);
    for i in (0..BC_X448_SIZE).rev() {
        value <<= 28usize;
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => value += num_bigint::BigInt::from(v),
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "X448Field native: malformed limb array".into(),
                }
                .into())
            }
        }
    }
    Ok(value)
}

pub(crate) fn bc_x448_reduce_to_uint(value: num_bigint::BigInt) -> num_bigint::BigUint {
    use num_bigint::ToBigUint;

    let modulus = bc_x448_modulus();
    let mut reduced = value % modulus;
    if reduced < num_bigint::BigInt::from(0i32) {
        reduced += modulus;
    }
    reduced
        .to_biguint()
        .unwrap_or_else(|| num_bigint::BigUint::from(0u32))
}

pub(crate) fn bc_x448_write_field_value(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    value: num_bigint::BigInt,
) -> Result<(), MethodCallFailed> {
    bc_pack_check_int_array(ctx, arr, 0, BC_X448_SIZE)?;
    let mask = num_bigint::BigUint::from(BC_X448_M28);
    let mut reduced = bc_x448_reduce_to_uint(value);
    for i in 0..BC_X448_SIZE {
        let limb = (&reduced & &mask)
            .to_u32_digits()
            .first()
            .copied()
            .unwrap_or(0);
        ctx.set_array_element(arr, i, Value::Int(limb as i32));
        reduced >>= 28usize;
    }
    Ok(())
}

pub(crate) fn bc_x448_field_mul(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let y_arr = obj_arg(args, 1)?;
    let z_arr = obj_arg(args, 2)?;
    let x = bc_x448_read_field_value(ctx, x_arr)?;
    let y = bc_x448_read_field_value(ctx, y_arr)?;
    bc_x448_write_field_value(ctx, z_arr, x * y)?;
    Ok(None)
}

pub(crate) fn bc_x448_field_mul_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let y = bc_pack_int_arg(args, 1);
    let z_arr = obj_arg(args, 2)?;
    let x = bc_x448_read_field_value(ctx, x_arr)?;
    bc_x448_write_field_value(ctx, z_arr, x * num_bigint::BigInt::from(y))?;
    Ok(None)
}

pub(crate) fn bc_x448_field_sqr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let z_arr = obj_arg(args, 1)?;
    let x = bc_x448_read_field_value(ctx, x_arr)?;
    bc_x448_write_field_value(ctx, z_arr, &x * &x)?;
    Ok(None)
}

pub(crate) fn bc_x448_field_sqr_n(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let n = bc_pack_int_arg(args, 1);
    let z_arr = obj_arg(args, 2)?;
    let modulus = bc_x448_modulus();
    let mut value = bc_x448_read_field_value(ctx, x_arr)? % modulus;
    if value < num_bigint::BigInt::from(0i32) {
        value += modulus;
    }
    let count = n.max(1) as usize;
    for _ in 0..count {
        value = (&value * &value) % modulus;
    }
    bc_x448_write_field_value(ctx, z_arr, value)?;
    Ok(None)
}

pub(crate) fn register_bc_x448_field(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let cls = "org/bouncycastle/math/ec/rfc7748/X448Field";
    r.register(cls, "mul", "([I[I[I)V", bc_x448_field_mul);
    r.register(cls, "mul", "([II[I)V", bc_x448_field_mul_int);
    r.register(cls, "sqr", "([I[I)V", bc_x448_field_sqr);
    r.register(cls, "sqr", "([II[I)V", bc_x448_field_sqr_n);
    r.set_category(__prev_cat);
}

pub(crate) const BC_BLAKE2S_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub(crate) const BC_BLAKE2S_SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

pub(crate) fn bc_blake2s_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_blake2s_int_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Result<u32, MethodCallFailed> {
    match ctx.get_field_by_name(this, name) {
        Value::Int(v) => Ok(v as u32),
        _ => Err(bc_blake2s_bad_state("Blake2sDigest: malformed int field")),
    }
}

pub(crate) fn bc_blake2s_int_array_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
    min_len: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = match ctx.get_field_by_name(this, name) {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_blake2s_bad_state("Blake2sDigest: missing array field")),
    };
    if ctx.array_length(arr) < min_len {
        return Err(RuntimeError::aioobe_index_only(min_len as i32).into());
    }
    Ok(arr)
}

pub(crate) fn bc_blake2s_read_int_array<const N: usize>(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    label: &str,
) -> Result<[u32; N], MethodCallFailed> {
    let mut out = [0u32; N];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("Blake2sDigest: malformed {label}"),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub(crate) fn bc_blake2s_write_int_array(ctx: &dyn NativeContext, arr: ObjectRef, words: &[u32]) {
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(word as i32));
    }
}

pub(crate) fn bc_blake2s_g(
    state: &mut [u32; 16],
    m1: u32,
    m2: u32,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(m1);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(m2);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

pub(crate) fn register_bc_blake2s_digest(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        "org/bouncycastle/crypto/digests/Blake2sDigest",
        "compress",
        "([BI)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let message_arr = obj_arg(args, 1)?;
            let message_pos = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
                _ => return Err(RuntimeError::aioobe_index_only(-1).into()),
            };
            let end = match message_pos.checked_add(64) {
                Some(v) => v,
                None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
            };
            if ctx.array_length(message_arr) < end {
                return Err(RuntimeError::aioobe_index_only(end as i32).into());
            }

            let chain_arr = bc_blake2s_int_array_field(ctx, this, "chainValue", 8)?;
            let state_arr = bc_blake2s_int_array_field(ctx, this, "internalState", 16)?;
            let mut chain = bc_blake2s_read_int_array::<8>(ctx, chain_arr, "chainValue")?;
            let t0 = bc_blake2s_int_field(ctx, this, "t0")?;
            let t1 = bc_blake2s_int_field(ctx, this, "t1")?;
            let f0 = bc_blake2s_int_field(ctx, this, "f0")?;
            let f1 = bc_blake2s_int_field(ctx, this, "f1")?;

            let mut block = [0u8; 64];
            ctx.read_byte_array_into(message_arr, message_pos, &mut block);
            let mut m = [0u32; 16];
            for (i, word) in m.iter_mut().enumerate() {
                let off = i * 4;
                *word = u32::from_le_bytes([
                    block[off],
                    block[off + 1],
                    block[off + 2],
                    block[off + 3],
                ]);
            }

            let mut state = [0u32; 16];
            state[..8].copy_from_slice(&chain);
            state[8..12].copy_from_slice(&BC_BLAKE2S_IV[..4]);
            state[12] = t0 ^ BC_BLAKE2S_IV[4];
            state[13] = t1 ^ BC_BLAKE2S_IV[5];
            state[14] = f0 ^ BC_BLAKE2S_IV[6];
            state[15] = f1 ^ BC_BLAKE2S_IV[7];

            for sigma in BC_BLAKE2S_SIGMA {
                bc_blake2s_g(&mut state, m[sigma[0]], m[sigma[1]], 0, 4, 8, 12);
                bc_blake2s_g(&mut state, m[sigma[2]], m[sigma[3]], 1, 5, 9, 13);
                bc_blake2s_g(&mut state, m[sigma[4]], m[sigma[5]], 2, 6, 10, 14);
                bc_blake2s_g(&mut state, m[sigma[6]], m[sigma[7]], 3, 7, 11, 15);
                bc_blake2s_g(&mut state, m[sigma[8]], m[sigma[9]], 0, 5, 10, 15);
                bc_blake2s_g(&mut state, m[sigma[10]], m[sigma[11]], 1, 6, 11, 12);
                bc_blake2s_g(&mut state, m[sigma[12]], m[sigma[13]], 2, 7, 8, 13);
                bc_blake2s_g(&mut state, m[sigma[14]], m[sigma[15]], 3, 4, 9, 14);
            }

            for i in 0..8 {
                chain[i] ^= state[i] ^ state[i + 8];
            }
            bc_blake2s_write_int_array(ctx, chain_arr, &chain);
            bc_blake2s_write_int_array(ctx, state_arr, &state);
            Ok(None)
        },
    );

    r.register(
        "org/bouncycastle/crypto/digests/Blake2sDigest",
        "G",
        "(IIIIII)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m1 = match args.get(1) {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let m2 = match args.get(2) {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let pos = |idx: usize| -> Result<usize, MethodCallFailed> {
                match args.get(idx) {
                    Some(Value::Int(v)) if (0..16).contains(v) => Ok(*v as usize),
                    Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
                    _ => Err(RuntimeError::aioobe_index_only(-1).into()),
                }
            };
            let pos_a = pos(3)?;
            let pos_b = pos(4)?;
            let pos_c = pos(5)?;
            let pos_d = pos(6)?;
            let state_arr = match ctx.get_field_by_name(this, "internalState") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "Blake2sDigest.G: missing internalState".into(),
                    }
                    .into())
                }
            };
            if ctx.array_length(state_arr) < 16 {
                return Err(RuntimeError::aioobe_index_only(16).into());
            }
            let mut state = [0u32; 16];
            for (i, slot) in state.iter_mut().enumerate() {
                match ctx.get_array_element(state_arr, i) {
                    Value::Int(v) => *slot = v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "Blake2sDigest.G: malformed internalState".into(),
                        }
                        .into())
                    }
                }
            }

            bc_blake2s_g(&mut state, m1, m2, pos_a, pos_b, pos_c, pos_d);

            for (i, &word) in state.iter().enumerate() {
                ctx.set_array_element(state_arr, i, Value::Int(word as i32));
            }
            Ok(None)
        },
    );

    r.set_category(__prev_cat);
}

/// The eight chaining words `H1..H8`, in order, plus the two remaining fields
/// `processBlock` touches.
const BC_SHA256_STATE_FIELDS: [&str; 8] = ["H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8"];

/// Resolved heap slot indices for one `SHA256Digest` class: `(H1..H8, X, xOff)`.
#[derive(Clone, Copy)]
struct BcSha256Slots {
    h: [usize; 8],
    x: usize,
    x_off: usize,
}

/// Slot cache, keyed by the receiver's `ClassId`.
///
/// `get_field_by_name` takes the class-manager read lock and walks the class
/// hierarchy by name on every call; `processBlock` touches ten fields and is
/// called once per 64-byte block, so paying that eighteen times per block would
/// cost more than the bytecode this native replaces. The indices are a property
/// of the class layout, so they are resolved once and reused.
///
/// Keyed on `ClassId` rather than cached unconditionally because the same class
/// name can be loaded by two class loaders (two `ClassId`s, two layouts); a
/// mismatch simply re-resolves rather than reading the wrong slots.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions copy a
/// `Copy` payload out in the same statement; every `NativeContext` call in
/// `bc_sha256_slots` (`class_id_of_object`, `declared_fields`) runs with no
/// guard held.
static BC_SHA256_SLOTS: cratonvm_types::lock_order::OrderedPlRwLock<Option<(u32, BcSha256Slots)>> =
    cratonvm_types::lock_order::OrderedPlRwLock::new(
        None,
        cratonvm_types::lock_order::LockLevel::Scratch,
    );

fn bc_sha256_slots(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<BcSha256Slots, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let key = class_id.as_u32();
    if let Some((cached_key, slots)) = *BC_SHA256_SLOTS.read() {
        if cached_key == key {
            return Ok(slots);
        }
    }
    let bad = |what: &str| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: format!("SHA256Digest: cannot resolve field {what}"),
        }
        .into()
    };
    // `declared_fields` reports fields declared BY this class with an absolute
    // heap slot index; H1..H8, X and xOff are all declared on `SHA256Digest`
    // itself, so no super-class walk is needed.
    let fields = ctx.declared_fields(class_id);
    let index_of = |name: &str| -> Option<usize> {
        fields
            .iter()
            .find(|f| f.name == name && !f.is_static)
            .map(|f| f.slot_index)
    };
    let mut h = [0usize; 8];
    for (slot, name) in h.iter_mut().zip(BC_SHA256_STATE_FIELDS) {
        *slot = index_of(name).ok_or_else(|| bad(name))?;
    }
    let slots = BcSha256Slots {
        h,
        x: index_of("X").ok_or_else(|| bad("X"))?,
        x_off: index_of("xOff").ok_or_else(|| bad("xOff"))?,
    };
    *BC_SHA256_SLOTS.write() = Some((key, slots));
    Ok(slots)
}

/// Native `org.bouncycastle.crypto.digests.SHA256Digest.processBlock()`.
///
/// # Why this one and not `MessageDigest`
///
/// BouncyCastle's LMS/HSS (`pqc.crypto.lms`) builds `new SHA256Digest()`
/// directly — see that package's `DigestUtil.createDigest` — so this VM's
/// native JCA SHA-256 is on a path the workload never takes, and HotSpot has no
/// intrinsic for BouncyCastle's class either. Both VMs run the round schedule as
/// real bytecode; measurement put CratonVM at ~37x HotSpot on that kernel with
/// the JIT fully engaged and nothing stuck in the interpreter. This replaces the
/// one leaf that owns the cost.
///
/// # Why `processBlock` is the right seam
///
/// It is a `protected` leaf with no arguments and no calls out: every input is a
/// field of the receiver (`H1..H8`, `X`), and the kernel reproduces its exact
/// post-state including the expanded schedule left in `X[16..64]` and the
/// cleared `X[0..16]`. Buffering, padding, length encoding, `reset`, `copy` and
/// `getEncodedState` all stay real bytecode.
///
/// `SHA256Digest` has no subclasses in BouncyCastle, so the superclass walk in
/// `intercept_force_registered_native` cannot divert some other digest's
/// `processBlock` here. Tagged `Intrinsic`, not a stub: it computes the method's
/// exact result rather than standing in for it.
pub(crate) fn register_bc_sha256_digest(r: &mut NativeMethodRegistry) {
    // `register_with_kind` rather than the ambient `set_category` the older BC
    // registrars around this one use: it records `kind_stated`, so the census
    // can tell "somebody adjudicated this as an Intrinsic" from "this inherited
    // whatever category was ambient at the registration site".
    r.register_with_kind(
        "org/bouncycastle/crypto/digests/SHA256Digest",
        "processBlock",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let slots = bc_sha256_slots(ctx, this)?;

            let mut state = [0u32; 8];
            for (slot, index) in state.iter_mut().zip(slots.h) {
                match ctx.get_field(this, index) {
                    Value::Int(v) => *slot = v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "SHA256Digest: malformed chaining word".into(),
                        }
                        .into())
                    }
                }
            }

            let x_arr = match ctx.get_field(this, slots.x) {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "SHA256Digest: missing X".into(),
                    }
                    .into())
                }
            };

            // One bulk read of all 64 words rather than 64 `get_array_element`
            // round trips — the per-element path costs a virtual dispatch plus a
            // `Value` box per word, which is most of what this native exists to
            // remove. Only `X[0..16]` is live input; the tail is read so the
            // single bulk write-back below can restore the whole array. A short
            // read means `X` is not the 64-word `int[]` the class declares.
            let mut words = [0i32; 64];
            if ctx.read_int_array_into(x_arr, 0, &mut words) != 64 {
                return Err(RuntimeError::IllegalStateException {
                    message: "SHA256Digest: X is not a 64-word int[]".into(),
                }
                .into());
            }
            let mut x = [0u32; 64];
            for (dst, src) in x.iter_mut().zip(words.iter()) {
                *dst = *src as u32;
            }

            cratonvm_native_builtins_crypto::bc_digest::sha256_process_block(&mut state, &mut x);

            for (word, index) in state.iter().zip(slots.h) {
                ctx.set_field(this, index, Value::Int(*word as i32));
            }
            for (dst, src) in words.iter_mut().zip(x.iter()) {
                *dst = *src as i32;
            }
            ctx.write_int_array_from(x_arr, 0, &words);
            // BouncyCastle's `xOff = 0`, which `processWord` reads to decide
            // when the next block is full. Omitting it would leave the digest
            // permanently mid-block.
            ctx.set_field(this, slots.x_off, Value::Int(0));
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

pub(crate) const BC_KECCAK_ROUND_CONSTANTS: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

pub(crate) fn bc_keccak_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_keccak_state_array(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let state_arr = match ctx.get_field_by_name(this, "state") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_keccak_bad_state("KeccakDigest: missing state")),
    };
    if ctx.array_length(state_arr) < 25 {
        return Err(RuntimeError::aioobe_index_only(25).into());
    }
    Ok(state_arr)
}

pub(crate) fn bc_keccak_data_queue(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "dataQueue") {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_keccak_bad_state("KeccakDigest: missing dataQueue")),
    }
}

pub(crate) fn bc_keccak_int_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<i32, MethodCallFailed> {
    match ctx.get_field_by_name(this, field) {
        Value::Int(v) => Ok(v),
        _ => Err(bc_keccak_bad_state("KeccakDigest: malformed int field")),
    }
}

pub(crate) fn bc_keccak_read_state(
    ctx: &dyn NativeContext,
    state_arr: ObjectRef,
) -> Result<[u64; 25], MethodCallFailed> {
    let mut state = [0u64; 25];
    for (i, slot) in state.iter_mut().enumerate() {
        match ctx.get_array_element(state_arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_keccak_bad_state("KeccakDigest: malformed state")),
        }
    }
    Ok(state)
}

pub(crate) fn bc_keccak_write_state(
    ctx: &dyn NativeContext,
    state_arr: ObjectRef,
    state: &[u64; 25],
) {
    for (i, &word) in state.iter().enumerate() {
        ctx.set_array_element(state_arr, i, Value::Long(word as i64));
    }
}

pub(crate) fn bc_keccak_permute(state: &mut [u64; 25]) {
    let mut a00 = state[0];
    let mut a01 = state[1];
    let mut a02 = state[2];
    let mut a03 = state[3];
    let mut a04 = state[4];
    let mut a05 = state[5];
    let mut a06 = state[6];
    let mut a07 = state[7];
    let mut a08 = state[8];
    let mut a09 = state[9];
    let mut a10 = state[10];
    let mut a11 = state[11];
    let mut a12 = state[12];
    let mut a13 = state[13];
    let mut a14 = state[14];
    let mut a15 = state[15];
    let mut a16 = state[16];
    let mut a17 = state[17];
    let mut a18 = state[18];
    let mut a19 = state[19];
    let mut a20 = state[20];
    let mut a21 = state[21];
    let mut a22 = state[22];
    let mut a23 = state[23];
    let mut a24 = state[24];

    for rc in BC_KECCAK_ROUND_CONSTANTS {
        let mut c0 = a00 ^ a05 ^ a10 ^ a15 ^ a20;
        let mut c1 = a01 ^ a06 ^ a11 ^ a16 ^ a21;
        let c2 = a02 ^ a07 ^ a12 ^ a17 ^ a22;
        let c3 = a03 ^ a08 ^ a13 ^ a18 ^ a23;
        let c4 = a04 ^ a09 ^ a14 ^ a19 ^ a24;

        let d1 = c1.rotate_left(1) ^ c4;
        let d2 = c2.rotate_left(1) ^ c0;
        let d3 = c3.rotate_left(1) ^ c1;
        let d4 = c4.rotate_left(1) ^ c2;
        let d0 = c0.rotate_left(1) ^ c3;

        a00 ^= d1;
        a05 ^= d1;
        a10 ^= d1;
        a15 ^= d1;
        a20 ^= d1;
        a01 ^= d2;
        a06 ^= d2;
        a11 ^= d2;
        a16 ^= d2;
        a21 ^= d2;
        a02 ^= d3;
        a07 ^= d3;
        a12 ^= d3;
        a17 ^= d3;
        a22 ^= d3;
        a03 ^= d4;
        a08 ^= d4;
        a13 ^= d4;
        a18 ^= d4;
        a23 ^= d4;
        a04 ^= d0;
        a09 ^= d0;
        a14 ^= d0;
        a19 ^= d0;
        a24 ^= d0;

        c1 = a01.rotate_left(1);
        a01 = a06.rotate_left(44);
        a06 = a09.rotate_left(20);
        a09 = a22.rotate_left(61);
        a22 = a14.rotate_left(39);
        a14 = a20.rotate_left(18);
        a20 = a02.rotate_left(62);
        a02 = a12.rotate_left(43);
        a12 = a13.rotate_left(25);
        a13 = a19.rotate_left(8);
        a19 = a23.rotate_left(56);
        a23 = a15.rotate_left(41);
        a15 = a04.rotate_left(27);
        a04 = a24.rotate_left(14);
        a24 = a21.rotate_left(2);
        a21 = a08.rotate_left(55);
        a08 = a16.rotate_left(45);
        a16 = a05.rotate_left(36);
        a05 = a03.rotate_left(28);
        a03 = a18.rotate_left(21);
        a18 = a17.rotate_left(15);
        a17 = a11.rotate_left(10);
        a11 = a07.rotate_left(6);
        a07 = a10.rotate_left(3);
        a10 = c1;

        c0 = a00 ^ (!a01 & a02);
        c1 = a01 ^ (!a02 & a03);
        a02 ^= !a03 & a04;
        a03 ^= !a04 & a00;
        a04 ^= !a00 & a01;
        a00 = c0;
        a01 = c1;

        c0 = a05 ^ (!a06 & a07);
        c1 = a06 ^ (!a07 & a08);
        a07 ^= !a08 & a09;
        a08 ^= !a09 & a05;
        a09 ^= !a05 & a06;
        a05 = c0;
        a06 = c1;

        c0 = a10 ^ (!a11 & a12);
        c1 = a11 ^ (!a12 & a13);
        a12 ^= !a13 & a14;
        a13 ^= !a14 & a10;
        a14 ^= !a10 & a11;
        a10 = c0;
        a11 = c1;

        c0 = a15 ^ (!a16 & a17);
        c1 = a16 ^ (!a17 & a18);
        a17 ^= !a18 & a19;
        a18 ^= !a19 & a15;
        a19 ^= !a15 & a16;
        a15 = c0;
        a16 = c1;

        c0 = a20 ^ (!a21 & a22);
        c1 = a21 ^ (!a22 & a23);
        a22 ^= !a23 & a24;
        a23 ^= !a24 & a20;
        a24 ^= !a20 & a21;
        a20 = c0;
        a21 = c1;

        a00 ^= rc;
    }

    *state = [
        a00, a01, a02, a03, a04, a05, a06, a07, a08, a09, a10, a11, a12, a13, a14, a15, a16, a17,
        a18, a19, a20, a21, a22, a23, a24,
    ];
}

pub(crate) fn register_bc_keccak_digest(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let class_name = "org/bouncycastle/crypto/digests/KeccakDigest";

    r.register(class_name, "KeccakPermutation", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state_arr = bc_keccak_state_array(ctx, this)?;
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);
        Ok(None)
    });

    r.register(class_name, "KeccakAbsorb", "([BI)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let data_arr = obj_arg(args, 1)?;
        let off = match args.get(2) {
            Some(Value::Int(v)) if *v >= 0 => *v as usize,
            Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
            _ => return Err(RuntimeError::aioobe_index_only(-1).into()),
        };
        let rate = bc_keccak_int_field(ctx, this, "rate")?;
        let count = (rate as usize) >> 6;
        let byte_count = count * 8;
        if off
            .checked_add(byte_count)
            .map_or(true, |end| end > ctx.array_length(data_arr))
        {
            return Err(RuntimeError::aioobe_index_only(off as i32).into());
        }

        let state_arr = bc_keccak_state_array(ctx, this)?;
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        let mut block = vec![0u8; byte_count];
        ctx.read_byte_array_into(data_arr, off, &mut block);
        for (i, chunk) in block.chunks_exact(8).enumerate() {
            state[i] ^= u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
        }
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);
        Ok(None)
    });

    r.register(class_name, "KeccakExtract", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rate = bc_keccak_int_field(ctx, this, "rate")?;
        let count = (rate as usize) >> 6;
        let state_arr = bc_keccak_state_array(ctx, this)?;
        let data_queue = bc_keccak_data_queue(ctx, this)?;
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);

        let mut out = vec![0u8; count * 8];
        for i in 0..count {
            out[i * 8..i * 8 + 8].copy_from_slice(&state[i].to_le_bytes());
        }
        if !ctx.write_byte_array_from(data_queue, 0, &out) {
            return Err(RuntimeError::aioobe_index_only(out.len() as i32).into());
        }
        ctx.set_field_by_name(this, "bitsInQueue", Value::Int(rate));
        Ok(None)
    });

    r.set_category(__prev_cat);
}

pub(crate) fn bc_sic_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_sic_object_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_sic_bad_state("SICBlockCipher: malformed object field")),
    }
}

pub(crate) fn bc_sic_write_reset_counter(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    counter_arr: ObjectRef,
    iv_arr: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let bs = ctx.array_length(counter_arr);
    let iv_len = ctx.array_length(iv_arr);
    if iv_len > bs {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("CTR/SIC mode requires IV no greater than: {bs} bytes."),
        }
        .into());
    }
    let mut counter = vec![0u8; bs];
    if iv_len != 0 {
        ctx.read_byte_array_into(iv_arr, 0, &mut counter[..iv_len]);
    }
    ctx.write_byte_array_from(counter_arr, 0, &counter);
    ctx.set_field_by_name(this, "byteCount", Value::Int(0));
    Ok(counter)
}

pub(crate) fn bc_sic_add_blocks(counter: &mut [u8], mut blocks: u64) {
    let mut idx = counter.len();
    while blocks != 0 && idx != 0 {
        idx -= 1;
        let add = (blocks & 0xff) as u16;
        let sum = counter[idx] as u16 + add;
        counter[idx] = sum as u8;
        blocks = (blocks >> 8) + u64::from(sum >> 8);
    }
}

pub(crate) fn bc_sic_increment_counter(counter: &mut [u8]) {
    let mut idx = counter.len();
    while idx != 0 {
        idx -= 1;
        counter[idx] = counter[idx].wrapping_add(1);
        if counter[idx] != 0 {
            break;
        }
    }
}

pub(crate) fn bc_sic_check_counter_prefix(
    ctx: &dyn NativeContext,
    counter: &[u8],
    iv_arr: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let iv_len = ctx.array_length(iv_arr);
    if iv_len < counter.len() {
        let mut iv = vec![0u8; iv_len];
        ctx.read_byte_array_into(iv_arr, 0, &mut iv);
        for i in 0..iv_len {
            if counter[i] != iv[i] {
                return Err(bc_sic_bad_state("Counter in CTR/SIC mode out of range."));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SICBlockCipher's 2^64-block range bound
// ---------------------------------------------------------------------------
//
// `SICBlockCipher` enforces two DIFFERENT bounds, and the shims below used to
// implement one of them.
//
// * **Partial IV** (`IV.length < blockSize`): the counter may only occupy the
//   low `blockSize - IV.length` bytes, so the IV prefix must still match. That
//   is `bc_sic_check_counter_prefix`, and it was implemented.
// * **Full-block IV**: there is no prefix to compare, so the class tracks the
//   ADVANCE SINCE INIT instead and refuses at 2^64 blocks — `fullBlockIV`,
//   `guardByte`, `used` and the sticky `overflow` flag, checked per block by
//   `checkLastIncrement` and per skip/seekTo by `checkCounter`. Every one of
//   those four fields was invisible to these natives: `init` never wrote
//   `fullBlockIV`/`guardByte`, `reset` never cleared `used`/`overflow`, and
//   `processBytes`' inline `checkLastIncrement` carried the comment
//   "no-op when the IV fills the block", which is the branch it is not.
//
// The visible consequence, measured against HotSpot with the same bc-java jar
// (`SICPositionTest`, in `crypto.test.AllTests`): after `skip` correctly threw
// `Counter in CTR/SIC mode out of range.` — `skip` is Java and calls
// `checkCounter` — the very next `processBytes` produced KEYSTREAM instead of
// re-throwing, because the sticky flag the Java had just set was one this
// native never read. A CTR keystream reused past its bound is a two-time-pad,
// so this is a silent-wrong-crypto shape, not only a missing exception.
//
// The helpers below mirror `populateDelta`, `checkLastIncrement`,
// `checkCounter` and `incrementCounter` arm-for-arm. Cost per block is one byte
// compare against `guardByte`; `populateDelta` runs only when that matches.

/// Mirror of `SICBlockCipher.populateDelta`: big-endian modular subtraction of
/// the IV (zero-padded on the right) from `counter` into `res`, carrying the
/// borrow across every byte. True when any byte ABOVE the low 8-byte lane is
/// non-zero — the counter has advanced 2^64 blocks or more since init, or has
/// moved below its starting value.
fn bc_sic_populate_delta(counter: &[u8], iv: &[u8], lane_off: usize, res: &mut [u8]) -> bool {
    let mut borrow = 0i32;
    for i in (0..res.len()).rev() {
        let mut v = counter[i] as i32 - borrow;
        if i < iv.len() {
            v -= iv[i] as i32;
        }
        if v < 0 {
            v += 256;
            borrow = 1;
        } else {
            borrow = 0;
        }
        res[i] = v as u8;
    }
    res[..lane_off.min(res.len())].iter().any(|&b| b != 0)
}

/// The four range-bound fields of a `SICBlockCipher`, read once per native call
/// and written back only when they changed.
pub(crate) struct BcSicRange {
    pub lane_off: usize,
    pub guard_byte: u8,
    pub full_block_iv: bool,
    pub overflow: bool,
    pub used: u64,
    dirty: bool,
}

impl BcSicRange {
    pub(crate) fn read(ctx: &dyn NativeContext, this: ObjectRef) -> Self {
        let int_field = |name: &str| match ctx.get_field_by_name(this, name) {
            Value::Int(v) => v,
            _ => 0,
        };
        BcSicRange {
            lane_off: int_field("laneOff").max(0) as usize,
            guard_byte: int_field("guardByte") as u8,
            full_block_iv: int_field("fullBlockIV") != 0,
            overflow: int_field("overflow") != 0,
            used: match ctx.get_field_by_name(this, "used") {
                Value::Long(v) => v as u64,
                _ => 0,
            },
            dirty: false,
        }
    }

    pub(crate) fn write_back(&self, ctx: &mut dyn NativeContext, this: ObjectRef) {
        if !self.dirty {
            return;
        }
        ctx.set_field_by_name(this, "overflow", Value::Int(i32::from(self.overflow)));
        ctx.set_field_by_name(this, "used", Value::Long(self.used as i64));
    }

    /// Mirror of `SICBlockCipher.checkLastIncrement`, run before each keystream
    /// block is produced.
    fn check_last_increment(
        &mut self,
        counter: &[u8],
        iv: &[u8],
        delta: &mut [u8],
    ) -> Result<(), MethodCallFailed> {
        let out_of_range = || bc_sic_bad_state("Counter in CTR/SIC mode out of range.");
        if iv.len() < counter.len() {
            if counter[iv.len() - 1] != iv[iv.len() - 1] {
                return Err(out_of_range());
            }
        } else if self.lane_off > 0 {
            if self.overflow
                || (counter[self.lane_off - 1] == self.guard_byte
                    && bc_sic_populate_delta(counter, iv, self.lane_off, delta))
            {
                self.overflow = true;
                self.dirty = true;
                return Err(out_of_range());
            }
        } else if self.overflow {
            return Err(out_of_range());
        }
        Ok(())
    }

    /// Mirror of the `used`/`overflow` half of `SICBlockCipher.incrementCounter`.
    /// The counter bytes themselves are advanced by `bc_sic_increment_counter`.
    fn note_increment(&mut self) {
        if self.lane_off == 0 {
            self.used = self.used.wrapping_add(1);
            if self.used == 0 && self.full_block_iv {
                self.overflow = true;
            }
            self.dirty = true;
        }
    }

    /// Mirror of `SICBlockCipher.checkCounter`, run per `skip`/`seekTo`.
    fn check_counter(
        &mut self,
        counter: &[u8],
        iv: &[u8],
        delta: &mut [u8],
    ) -> Result<(), MethodCallFailed> {
        let out_of_range = || bc_sic_bad_state("Counter in CTR/SIC mode out of range.");
        if iv.len() < counter.len() {
            for i in (0..iv.len()).rev() {
                if counter[i] != iv[i] {
                    return Err(out_of_range());
                }
            }
        } else {
            if self.overflow || bc_sic_populate_delta(counter, iv, self.lane_off, delta) {
                self.overflow = true;
                self.dirty = true;
                return Err(out_of_range());
            }
            if self.lane_off == 0 {
                let n = delta.len();
                let mut acc = 0u64;
                for &b in &delta[n - 8..] {
                    acc = (acc << 8) | b as u64;
                }
                self.used = acc;
                self.dirty = true;
            }
        }
        Ok(())
    }
}

/// `reset()`'s share of the range state: `used = 0`, `overflow = false`.
/// Written unconditionally — this is the only path that CLEARS a sticky flag,
/// so it must not be elided by the dirty check.
fn bc_sic_reset_range(ctx: &mut dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, "used", Value::Long(0));
    ctx.set_field_by_name(this, "overflow", Value::Int(0));
}

pub(crate) fn bc_sic_encrypt_counter(
    ctx: &mut dyn NativeContext,
    cipher: ObjectRef,
    counter_arr: ObjectRef,
    counter_out_arr: ObjectRef,
    counter: &[u8],
) -> MethodCallResult {
    let is_aes = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(cipher))
        .as_deref()
        == Some("org/bouncycastle/crypto/engines/AESEngine");
    if is_aes {
        if let Value::Object(Some(wk)) = ctx.get_field_by_name(cipher, "WorkingKey") {
            let kw = read_aes_kw(ctx, wk);
            if kw.len() >= 2 {
                let mut out = vec![0u8; counter.len()];
                crate::bc_aes::encrypt_block(&kw, counter, &mut out);
                ctx.write_byte_array_from(counter_out_arr, 0, &out);
                return Ok(Some(Value::Int(counter.len() as i32)));
            }
        }
    }

    ctx.write_byte_array_from(counter_arr, 0, counter);
    ctx.invoke_virtual(
        cipher,
        "processBlock",
        "([BI[BI)I",
        &[
            Value::Object(Some(counter_arr)),
            Value::Int(0),
            Value::Object(Some(counter_out_arr)),
            Value::Int(0),
        ],
    )
}

/// Native fast-path for `org.bouncycastle.crypto.modes.SICBlockCipher.processBytes`
/// (CTR mode). Once the AES engine is native, this per-byte XOR loop is the sole
/// remaining hot frame in `AESTest.testCounter` (profiled: ~112s vs ~0.4s for the
/// String half). Faithful reimplementation of the bytecode loop:
///   if byteCount==0 { checkLastIncrement(); cipher.processBlock(counter,counterOut);
///                     next = in ^ counterOut[byteCount++]; }
///   else { next = in ^ counterOut[byteCount++];
///          if byteCount==counter.length { byteCount=0; incrementCounter(); } }
/// For AES (the common case) the keystream block is produced natively from the
/// engine's `WorkingKey` (no per-block invoke, GC-safe — all loop state is in Rust
/// locals). For any other underlying cipher it falls back to invoking
/// `cipher.processBlock` per block (handles held across the invoke, matching the
/// apps_h2.rs convention; the default young GC is non-moving). `checkLastIncrement`
/// is a no-op when the IV fills the block (AESTest's case).
pub(crate) fn register_bc_sic_ctr(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    r.register(
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off_i = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let out_arr = obj_arg(args, 3)?;
            let out_off_i = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let obj_field = |name: &str| match ctx.get_field_by_name(this, name) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let cipher = obj_field("cipher").ok_or_else(|| bc_sic_bad_state("SIC: null cipher"))?;
            let counter_arr =
                obj_field("counter").ok_or_else(|| bc_sic_bad_state("SIC: null counter"))?;
            let counter_out_arr =
                obj_field("counterOut").ok_or_else(|| bc_sic_bad_state("SIC: null counterOut"))?;
            let byte_count = match ctx.get_field_by_name(this, "byteCount") {
                Value::Int(v) => v,
                _ => 0,
            };
            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(bc_sic_bad_state("SIC: zero block size"));
            }
            if byte_count != 0 {
                return ctx.invoke_virtual(
                    this,
                    "processBytes",
                    "([BII[BI)I",
                    &[
                        Value::Object(Some(in_arr)),
                        Value::Int(in_off_i),
                        Value::Int(bs as i32),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                );
            }
            if in_off_i < 0
                || out_off_i < 0
                || (in_off_i as usize).saturating_add(bs) > ctx.array_length(in_arr)
            {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too small",
                ));
            }
            if (out_off_i as usize).saturating_add(bs) > ctx.array_length(out_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }

            let mut counter = vec![0u8; bs];
            ctx.read_byte_array_into(counter_arr, 0, &mut counter);
            // `processBlock` calls `checkLastIncrement` before producing a
            // block, exactly as `processBytes` does — this native did not, so a
            // counter past its bound still yielded keystream through this door
            // even after the sibling one was fixed. Same predicate, both doors.
            let iv_arr = obj_field("IV").ok_or_else(|| bc_sic_bad_state("SIC: null IV"))?;
            let iv_len = ctx.array_length(iv_arr);
            let mut iv = vec![0u8; iv_len];
            ctx.read_byte_array_into(iv_arr, 0, &mut iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta = vec![0u8; bs];
            if let Err(e) = range.check_last_increment(&counter, &iv, &mut delta) {
                range.write_back(ctx, this);
                return Err(e);
            }
            bc_sic_encrypt_counter(ctx, cipher, counter_arr, counter_out_arr, &counter)?;
            let mut counter_out = vec![0u8; bs];
            ctx.read_byte_array_into(counter_out_arr, 0, &mut counter_out);
            let mut input = vec![0u8; bs];
            ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
            for i in 0..bs {
                input[i] ^= counter_out[i];
            }
            ctx.write_byte_array_from(out_arr, out_off_i as usize, &input);
            bc_sic_increment_counter(&mut counter);
            range.note_increment();
            ctx.write_byte_array_from(counter_arr, 0, &counter);
            range.write_back(ctx, this);
            Ok(Some(Value::Int(bs as i32)))
        },
    );

    r.register(
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "processBytes",
        "([BII[BI)I",
        |ctx, args| {
            let arg_i32 = |i: usize| -> i32 {
                match args.get(i) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                }
            };
            let ise = |m: &str| -> MethodCallFailed {
                RuntimeError::IllegalStateException { message: m.into() }.into()
            };
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off = arg_i32(2);
            let len_i = arg_i32(3);
            let out_arr = obj_arg(args, 4)?;
            let out_off = arg_i32(5);

            let obj_field = |name: &str| match ctx.get_field_by_name(this, name) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let cipher = obj_field("cipher").ok_or_else(|| ise("SIC: null cipher"))?;
            let counter_arr = obj_field("counter").ok_or_else(|| ise("SIC: null counter"))?;
            let counter_out_arr =
                obj_field("counterOut").ok_or_else(|| ise("SIC: null counterOut"))?;
            let iv_arr = obj_field("IV").ok_or_else(|| ise("SIC: null IV"))?;
            let mut byte_count = match ctx.get_field_by_name(this, "byteCount") {
                Value::Int(v) => v,
                _ => 0,
            };

            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(ise("SIC: zero block size"));
            }
            // CTR misuse guard (BC throws DataLength/OutputLengthException; both
            // are RuntimeExceptions and this is a can't-happen path for valid use).
            if in_off < 0
                || len_i < 0
                || out_off < 0
                || (in_off as usize) + (len_i as usize) > ctx.array_length(in_arr)
                || (out_off as usize) + (len_i as usize) > ctx.array_length(out_arr)
            {
                return Err(ise("CTR/SIC buffer length out of range"));
            }
            let len = len_i as usize;

            let mut counter = vec![0u8; bs];
            ctx.read_byte_array_into(counter_arr, 0, &mut counter);
            let mut keystream = vec![0u8; bs];
            ctx.read_byte_array_into(counter_out_arr, 0, &mut keystream);
            let iv_len = ctx.array_length(iv_arr);
            let iv_last = if iv_len >= 1 && iv_len <= bs {
                match ctx.get_array_element(iv_arr, iv_len - 1) {
                    Value::Int(v) => v as u8,
                    _ => 0,
                }
            } else {
                0
            };

            let mut in_buf = vec![0u8; len];
            ctx.read_byte_array_into(in_arr, in_off as usize, &mut in_buf);
            let mut out_buf = vec![0u8; len];
            // The full-block-IV range state. `iv_last` above covers only the
            // partial-IV branch of `checkLastIncrement`; see `BcSicRange`.
            let mut iv = vec![0u8; iv_len];
            ctx.read_byte_array_into(iv_arr, 0, &mut iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta = vec![0u8; bs];

            // AES fast path: produce the keystream block natively from WorkingKey.
            let is_aes = ctx
                .class_name_arc_of_id(ctx.class_id_of_object(cipher))
                .as_deref()
                == Some("org/bouncycastle/crypto/engines/AESEngine");
            let kw: Vec<[u32; 4]> = if is_aes {
                match ctx.get_field_by_name(cipher, "WorkingKey") {
                    Value::Object(Some(wk)) => read_aes_kw(ctx, wk),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            let use_aes = kw.len() >= 2;

            let _ = iv_last;
            // GC-safety: the non-AES arm below dispatches `processBlock` -- real
            // BouncyCastle bytecode -- once per block, and `cipher` and both
            // counter arrays are bare Rust locals bound before the loop. From
            // the second block on the dispatch and the two array arguments are
            // pre-GC addresses.
            let cipher_pin = ctx.pin_native_root(cipher);
            let counter_pin = ctx.pin_native_root(counter_arr);
            let counter_out_pin = ctx.pin_native_root(counter_out_arr);
            for i in 0..len {
                let cipher = ctx.read_native_pin(cipher_pin, cipher);
                let counter_arr = ctx.read_native_pin(counter_pin, counter_arr);
                let counter_out_arr = ctx.read_native_pin(counter_out_pin, counter_out_arr);
                let next;
                if byte_count == 0 {
                    // checkLastIncrement — BOTH branches. Anything already
                    // produced into `out_buf` is discarded on the throw, exactly
                    // as the Java loop's partial output is.
                    if let Err(e) = range.check_last_increment(&counter, &iv, &mut delta) {
                        range.write_back(ctx, this);
                        return Err(e);
                    }
                    if use_aes {
                        crate::bc_aes::encrypt_block(&kw, &counter, &mut keystream);
                    } else {
                        ctx.write_byte_array_from(counter_arr, 0, &counter);
                        ctx.invoke_virtual(
                            cipher,
                            "processBlock",
                            "([BI[BI)I",
                            &[
                                Value::Object(Some(counter_arr)),
                                Value::Int(0),
                                Value::Object(Some(counter_out_arr)),
                                Value::Int(0),
                            ],
                        )?;
                        ctx.read_byte_array_into(counter_out_arr, 0, &mut keystream);
                    }
                    next = in_buf[i] ^ keystream[byte_count as usize];
                    byte_count += 1;
                } else {
                    next = in_buf[i] ^ keystream[byte_count as usize];
                    byte_count += 1;
                    if byte_count as usize == bs {
                        byte_count = 0;
                        // incrementCounter (big-endian, from the last byte)
                        let mut j = bs;
                        while j > 0 {
                            j -= 1;
                            counter[j] = counter[j].wrapping_add(1);
                            if counter[j] != 0 {
                                break;
                            }
                        }
                        // …and its `used`/`overflow` half, which is what
                        // detects the wrap for a block size of 8 or less.
                        range.note_increment();
                    }
                }
                out_buf[i] = next;
            }

            // Persist mutated state + output, through the pins: the loop
            // above ran bytecode.
            let counter_arr = ctx.read_native_pin(counter_pin, counter_arr);
            let counter_out_arr = ctx.read_native_pin(counter_out_pin, counter_out_arr);
            ctx.unpin_native_roots(cipher_pin);
            ctx.write_byte_array_from(counter_arr, 0, &counter);
            ctx.write_byte_array_from(counter_out_arr, 0, &keystream);
            ctx.set_field_by_name(this, "byteCount", Value::Int(byte_count));
            range.write_back(ctx, this);
            ctx.write_byte_array_from(out_arr, out_off as usize, &out_buf);
            Ok(Some(Value::Int(len as i32)))
        },
    );

    r.set_category(__prev_cat);
}

/// Intrinsics for `org.bouncycastle.util.Strings` UTF-8 transcode. With BC
/// JIT-banned these otherwise run interpreted through `UTF8.transcodeToUTF16`
/// and the slow `new String(char[])` / `StringUTF16.compress` path, which
/// dominates `AESTest.testCounter` (255k growing-string round-trips) once the
/// AES engine is native. Strict UTF-8 (RFC 3629) matches BC's decoder for valid
/// input and raises the same `IllegalArgumentException("Invalid UTF-8 input")`
/// for invalid input; encoding a (valid) Java String yields standard UTF-8.
pub(crate) fn register_bc_strings_utf8(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let s = "org/bouncycastle/util/Strings";

    r.register(
        s,
        "fromUTF8ByteArray",
        "([B)Ljava/lang/String;",
        |ctx, args| {
            let arr = obj_arg(args, 0)?;
            let len = ctx.array_length(arr);
            let mut buf = vec![0u8; len];
            ctx.read_byte_array_into(arr, 0, &mut buf);
            match std::str::from_utf8(&buf) {
                // BC's `new String(chars, 0, len)` is a fresh, DISTINCT, un-interned
                // object — must NOT go through the pooling `create_string`, which
                // would (a) leak every dynamically-decoded string into the intern
                // pool (testCounter decodes 255k unique growing strings → heap
                // exhaustion) and (b) give wrong `==` identity semantics.
                Ok(text) => Ok(Some(Value::Object(Some(
                    ctx.create_string_uninterned(text),
                )))),
                Err(_) => Err(RuntimeError::IllegalArgumentException {
                    message: "Invalid UTF-8 input".into(),
                }
                .into()),
            }
        },
    );

    r.register(
        s,
        "toUTF8ByteArray",
        "(Ljava/lang/String;)[B",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let text = ctx.read_string(str_obj).unwrap_or_default();
            let bytes = text.as_bytes();
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
            ctx.write_byte_array_from(arr, 0, bytes);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    r.set_category(__prev_cat);
}

/// BouncyCastle byte-array helpers used heavily by the crypto regression tests.
/// `AESTest.testCounter` appends to a synthetic file with
/// `org.bouncycastle.util.Arrays.copyOf(byte[], int)` three times per verify.
/// The Java body is tiny, but under the standing `org/bouncycastle/` JIT ban the
/// call dispatch and bytecode scaffolding dominate hundreds of thousands of
/// short copies.
pub(crate) fn register_bc_arrays_helpers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let arrays = "org/bouncycastle/util/Arrays";

    r.register(arrays, "copyOf", "([BI)[B", |ctx, args| {
        let original = obj_arg(args, 0)?;
        let new_len_i = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if new_len_i < 0 {
            return Err(RuntimeError::NegativeArraySizeException { size: new_len_i }.into());
        }
        let new_len = new_len_i as usize;
        let copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, new_len);
        let copy_len = ctx.array_length(original).min(new_len);
        if copy_len != 0 {
            let mut buf = vec![0u8; copy_len];
            ctx.read_byte_array_into(original, 0, &mut buf);
            ctx.write_byte_array_from(copy, 0, &buf);
        }
        Ok(Some(Value::Object(Some(copy))))
    });

    r.set_category(__prev_cat);
}

pub(crate) fn bc_clone_byte_array_range(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    off: i32,
    len: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if off < 0 {
        return Err(RuntimeError::aioobe_index_only(off).into());
    }
    if len < 0 {
        return Err(RuntimeError::aioobe_index_only(len).into());
    }
    let off = off as usize;
    let len = len as usize;
    let src_len = ctx.array_length(src);
    let Some(end) = off.checked_add(len) else {
        return Err(RuntimeError::aioobe_index_only(i32::MAX).into());
    };
    if end > src_len {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    if len != 0 {
        let mut bytes = vec![0u8; len];
        ctx.read_byte_array_into(src, off, &mut bytes);
        ctx.write_byte_array_from(dst, 0, &bytes);
    }
    Ok(dst)
}

pub(crate) fn register_bc_param_helpers(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let key_parameter = "org/bouncycastle/crypto/params/KeyParameter";
    r.register(key_parameter, "<init>", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = obj_arg(args, 1)?;
        let len = ctx.array_length(key).min(i32::MAX as usize) as i32;
        let copy = bc_clone_byte_array_range(ctx, key, 0, len)?;
        ctx.set_field_by_name(this, "key", Value::Object(Some(copy)));
        Ok(None)
    });
    r.register(key_parameter, "<init>", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = obj_arg(args, 1)?;
        let off = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let copy = bc_clone_byte_array_range(ctx, key, off, len)?;
        ctx.set_field_by_name(this, "key", Value::Object(Some(copy)));
        Ok(None)
    });

    let params_iv = "org/bouncycastle/crypto/params/ParametersWithIV";
    r.register(
        params_iv,
        "<init>",
        "(Lorg/bouncycastle/crypto/CipherParameters;[B)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = args.get(1).copied().unwrap_or(Value::Object(None));
            let iv = obj_arg(args, 2)?;
            let len = ctx.array_length(iv).min(i32::MAX as usize) as i32;
            let copy = bc_clone_byte_array_range(ctx, iv, 0, len)?;
            ctx.set_field_by_name(this, "parameters", params);
            ctx.set_field_by_name(this, "iv", Value::Object(Some(copy)));
            Ok(None)
        },
    );
    r.register(
        params_iv,
        "<init>",
        "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = args.get(1).copied().unwrap_or(Value::Object(None));
            let iv = obj_arg(args, 2)?;
            let off = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let len = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let copy = bc_clone_byte_array_range(ctx, iv, off, len)?;
            ctx.set_field_by_name(this, "parameters", params);
            ctx.set_field_by_name(this, "iv", Value::Object(Some(copy)));
            Ok(None)
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_scrypt_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let mut out = vec![0u8; ctx.array_length(arr)];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub(crate) fn bc_scrypt_illegal(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_scrypt_le_bytes_to_i32(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(crate) fn bc_scrypt_i32_to_le_bytes(words: &[i32], out: &mut [u8]) {
    for (i, &word) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
}

pub(crate) fn bc_scrypt_block_mix(
    b: &[i32],
    x1: &mut [i32; 16],
    x2: &mut [i32; 16],
    y: &mut [i32],
    r: usize,
) {
    x1.copy_from_slice(&b[b.len() - 16..]);
    let mut b_off = 0usize;
    let mut y_off = 0usize;
    let half_len = b.len() >> 1;
    for _ in 0..(2 * r) {
        for i in 0..16 {
            x2[i] = x1[i] ^ b[b_off + i];
        }
        crate::bc_chacha::salsa_core(8, x2, x1);
        y[y_off..y_off + 16].copy_from_slice(x1);
        y_off = half_len + b_off - y_off;
        b_off += 16;
    }
}

pub(crate) fn bc_scrypt_smix(b: &mut [i32], b_off: usize, n: usize, d: usize, r: usize) {
    let pow_n = n.trailing_zeros() as usize;
    let blocks_per_chunk = n >> d;
    let chunk_count = 1usize << d;
    let chunk_mask = blocks_per_chunk - 1;
    let chunk_pow = pow_n - d;
    let b_count = r * 32;

    let mut block_x1 = [0i32; 16];
    let mut block_x2 = [0i32; 16];
    let mut block_y = vec![0i32; b_count];
    let mut x = vec![0i32; b_count];
    let mut vv = Vec::with_capacity(chunk_count);
    x.copy_from_slice(&b[b_off..b_off + b_count]);

    for _ in 0..chunk_count {
        let mut v = vec![0i32; blocks_per_chunk * b_count];
        let mut off = 0usize;
        for _ in (0..blocks_per_chunk).step_by(2) {
            v[off..off + b_count].copy_from_slice(&x);
            off += b_count;
            bc_scrypt_block_mix(&x, &mut block_x1, &mut block_x2, &mut block_y, r);
            v[off..off + b_count].copy_from_slice(&block_y);
            off += b_count;
            bc_scrypt_block_mix(&block_y, &mut block_x1, &mut block_x2, &mut x, r);
        }
        vv.push(v);
    }

    let mask = n - 1;
    for _ in 0..n {
        let j = (x[b_count - 16] as usize) & mask;
        let v = &vv[j >> chunk_pow];
        let v_off = (j & chunk_mask) * b_count;
        block_y.copy_from_slice(&v[v_off..v_off + b_count]);
        for i in 0..b_count {
            block_y[i] ^= x[i];
        }
        bc_scrypt_block_mix(&block_y, &mut block_x1, &mut block_x2, &mut x, r);
    }

    b[b_off..b_off + b_count].copy_from_slice(&x);
}

pub(crate) fn bc_scrypt_generate_bytes(
    password: &[u8],
    salt: &[u8],
    n: usize,
    r: usize,
    p: usize,
    dk_len: usize,
) -> Vec<u8> {
    let mf_len_bytes = r * 128;
    let mut bytes =
        crate::phases_early::pbkdf2_derive_for(256, password, salt, 1, p * mf_len_bytes);
    let mut b = bc_scrypt_le_bytes_to_i32(&bytes);

    let mut d = 0usize;
    let mut total = n * r;
    while n.saturating_sub(d) > 2 && total > (1 << 10) {
        d += 1;
        total >>= 1;
    }

    let mf_len_words = mf_len_bytes >> 2;
    for b_off in (0..b.len()).step_by(mf_len_words) {
        bc_scrypt_smix(&mut b, b_off, n, d, r);
    }

    bc_scrypt_i32_to_le_bytes(&b, &mut bytes);
    crate::phases_early::pbkdf2_derive_for(256, password, &bytes, 1, dk_len)
}

pub(crate) fn register_bc_scrypt_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/crypto/generators/SCrypt",
        "generate",
        "([B[BIIII)[B",
        |ctx, args| {
            let p_arr = obj_arg(args, 0)?;
            let s_arr = obj_arg(args, 1)?;
            let n = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let r_param = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let p_param = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let dk_len = match args.get(5) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };

            if n <= 1 || (n & (n - 1)) != 0 {
                return Err(bc_scrypt_illegal("Cost parameter N must be > 1 and a power of 2"));
            }
            if r_param == 1 && n >= 65536 {
                return Err(bc_scrypt_illegal("Cost parameter N must be > 1 and < 65536."));
            }
            if r_param < 1 {
                return Err(bc_scrypt_illegal("Block size r must be >= 1."));
            }
            let max_parallel = i32::MAX / (128 * r_param * 8);
            if p_param < 1 || p_param > max_parallel {
                return Err(bc_scrypt_illegal(&format!(
                    "Parallelisation parameter p must be >= 1 and <= {max_parallel} (based on block size r of {r_param})"
                )));
            }
            if dk_len < 1 {
                return Err(bc_scrypt_illegal("Generated key length dkLen must be >= 1."));
            }

            let password = bc_scrypt_read_byte_array(ctx, p_arr);
            let salt = bc_scrypt_read_byte_array(ctx, s_arr);
            let derived = bc_scrypt_generate_bytes(
                &password,
                &salt,
                n as usize,
                r_param as usize,
                p_param as usize,
                dk_len as usize,
            );
            let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, derived.len());
            ctx.write_byte_array_from(out, 0, &derived);
            Ok(Some(Value::Object(Some(out))))
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) const BC_ARGON2_BLOCK_QWORDS: usize = 128;

pub(crate) const BC_ARGON2_M32L: u64 = 0xffff_ffff;

pub(crate) fn bc_argon2_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_argon2_bad_argument(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_argon2_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let mut out = vec![0u8; ctx.array_length(arr)];
    ctx.read_byte_array_into(arr, 0, &mut out);
    out
}

pub(crate) fn bc_argon2_read_optional_byte_array(ctx: &dyn NativeContext, value: Value) -> Vec<u8> {
    match value {
        Value::Object(Some(arr)) => bc_argon2_read_byte_array(ctx, arr),
        _ => Vec::new(),
    }
}

pub(crate) fn bc_argon2_read_int_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    name: &str,
) -> Result<i32, MethodCallFailed> {
    match ctx.get_field_by_name(obj, name) {
        Value::Int(v) => Ok(v),
        _ => Err(bc_argon2_bad_state("Argon2Parameters: malformed int field")),
    }
}

pub(crate) fn bc_argon2_class_name(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<String, MethodCallFailed> {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .ok_or_else(|| bc_argon2_bad_state("Argon2 BlockPool: unknown receiver class"))
}

pub(crate) fn bc_argon2_exercise_block_pool(
    ctx: &mut dyn NativeContext,
    pool: ObjectRef,
    memory_blocks: usize,
) -> Result<(), MethodCallFailed> {
    let block_desc = "Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;";
    let allocate_desc = format!("(){block_desc}");
    let deallocate_desc = format!("({block_desc})V");
    let pool_base = ctx.pin_native_root(pool);

    let result = (|| -> Result<(), MethodCallFailed> {
        let total_blocks = memory_blocks
            .checked_add(4)
            .ok_or_else(|| bc_argon2_bad_argument("Argon2 block count overflow"))?;
        let mut blocks = Vec::with_capacity(total_blocks);

        for _ in 0..total_blocks {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let block = match ctx.invoke(
                &pool_class,
                "allocate",
                &allocate_desc,
                &[Value::Object(Some(pool_now))],
            )? {
                Some(Value::Object(Some(block))) => block,
                _ => {
                    return Err(bc_argon2_bad_state(
                        "Argon2 BlockPool.allocate returned null",
                    ))
                }
            };
            let block_pin = ctx.pin_native_root(block);
            blocks.push((block_pin, block));
        }

        let filler_base = memory_blocks;
        for index in [
            filler_base + 2,
            filler_base + 3,
            filler_base,
            filler_base + 1,
        ] {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let (block_pin, block) = blocks[index];
            let block_now = ctx.read_native_pin(block_pin, block);
            ctx.invoke(
                &pool_class,
                "deallocate",
                &deallocate_desc,
                &[
                    Value::Object(Some(pool_now)),
                    Value::Object(Some(block_now)),
                ],
            )?;
        }

        for &(block_pin, block) in blocks.iter().take(memory_blocks) {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let block_now = ctx.read_native_pin(block_pin, block);
            ctx.invoke(
                &pool_class,
                "deallocate",
                &deallocate_desc,
                &[
                    Value::Object(Some(pool_now)),
                    Value::Object(Some(block_now)),
                ],
            )?;
        }
        Ok(())
    })();

    ctx.unpin_native_roots(pool_base);
    result
}

pub(crate) fn bc_argon2_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let words = match ctx.get_field_by_name(block, "v") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(bc_argon2_bad_state(
                "Argon2BytesGenerator.Block: missing word array",
            ))
        }
    };
    if ctx.array_length(words) < BC_ARGON2_BLOCK_QWORDS {
        return Err(RuntimeError::aioobe_index_only(BC_ARGON2_BLOCK_QWORDS as i32).into());
    }
    Ok(words)
}

pub(crate) fn bc_argon2_read_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<[u64; BC_ARGON2_BLOCK_QWORDS], MethodCallFailed> {
    let words_arr = bc_argon2_block_words(ctx, block)?;
    let mut words = [0u64; BC_ARGON2_BLOCK_QWORDS];
    for (i, slot) in words.iter_mut().enumerate() {
        match ctx.get_array_element(words_arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_argon2_bad_state("Argon2 block word array is malformed")),
        }
    }
    Ok(words)
}

pub(crate) fn bc_argon2_write_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
    words: &[u64; BC_ARGON2_BLOCK_QWORDS],
) -> Result<(), MethodCallFailed> {
    let words_arr = bc_argon2_block_words(ctx, block)?;
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(words_arr, i, Value::Long(word as i64));
    }
    Ok(())
}

pub(crate) fn bc_argon2_clear_block(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<(), MethodCallFailed> {
    bc_argon2_write_block_words(ctx, block, &[0u64; BC_ARGON2_BLOCK_QWORDS])
}

pub(crate) fn bc_argon2_alloc_block(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let block_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block";
    let class_id = match ctx.ensure_class_initialized(block_class) {
        Ok(cid) => cid,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): `Argon2BytesGenerator$Block`
        // is a Bouncy Castle class, so a fabricated stand-in for it is a
        // dependency substitution — exactly what contract §5 refuses. On a run
        // that actually has BC on the classpath the `Ok` arm is what runs.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, block_class, 1)?,
    };
    let n_fields = ctx.class_num_total_fields(class_id).max(1);
    let block = ctx.alloc_object(class_id, n_fields);
    let words = ctx.new_array(
        cratonvm_types::ArrayElementType::Long,
        BC_ARGON2_BLOCK_QWORDS,
    );
    ctx.set_field_by_name(block, "v", Value::Object(Some(words)));
    Ok(block)
}

pub(crate) fn bc_argon2_fillblock_block_field(
    ctx: &dyn NativeContext,
    fill_block: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(fill_block, name) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_argon2_bad_state("Argon2 FillBlock: missing block field")),
    }
}

pub(crate) fn bc_argon2_round_index(args: &[Value], idx: usize) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if (0..BC_ARGON2_BLOCK_QWORDS as i32).contains(v) => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Err(RuntimeError::aioobe_index_only(-1).into()),
    }
}

pub(crate) fn bc_argon2_quarter_round(
    v: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    x: usize,
    y: usize,
    z: usize,
    s: u32,
) {
    let mut a = v[x];
    let b = v[y];
    let mut c = v[z];
    a = a.wrapping_add(b).wrapping_add(
        2u64.wrapping_mul(a & BC_ARGON2_M32L)
            .wrapping_mul(b & BC_ARGON2_M32L),
    );
    c = (c ^ a).rotate_right(s);
    v[x] = a;
    v[z] = c;
}

pub(crate) fn bc_argon2_f(
    v: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) {
    bc_argon2_quarter_round(v, a, b, d, 32);
    bc_argon2_quarter_round(v, c, d, b, 24);
    bc_argon2_quarter_round(v, a, b, d, 16);
    bc_argon2_quarter_round(v, c, d, b, 63);
}

pub(crate) fn bc_argon2_round_function_words(
    words: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    v0: usize,
    v1: usize,
    v2: usize,
    v3: usize,
    v4: usize,
    v5: usize,
    v6: usize,
    v7: usize,
    v8: usize,
    v9: usize,
    v10: usize,
    v11: usize,
    v12: usize,
    v13: usize,
    v14: usize,
    v15: usize,
) {
    bc_argon2_f(words, v0, v4, v8, v12);
    bc_argon2_f(words, v1, v5, v9, v13);
    bc_argon2_f(words, v2, v6, v10, v14);
    bc_argon2_f(words, v3, v7, v11, v15);

    bc_argon2_f(words, v0, v5, v10, v15);
    bc_argon2_f(words, v1, v6, v11, v12);
    bc_argon2_f(words, v2, v7, v8, v13);
    bc_argon2_f(words, v3, v4, v9, v14);
}

pub(crate) fn bc_argon2_apply_blake_words(words: &mut [u64; BC_ARGON2_BLOCK_QWORDS]) {
    for i in 0..8 {
        let i16 = 16 * i;
        bc_argon2_round_function_words(
            words,
            i16,
            i16 + 1,
            i16 + 2,
            i16 + 3,
            i16 + 4,
            i16 + 5,
            i16 + 6,
            i16 + 7,
            i16 + 8,
            i16 + 9,
            i16 + 10,
            i16 + 11,
            i16 + 12,
            i16 + 13,
            i16 + 14,
            i16 + 15,
        );
    }

    for i in 0..8 {
        let i2 = 2 * i;
        bc_argon2_round_function_words(
            words,
            i2,
            i2 + 1,
            i2 + 16,
            i2 + 17,
            i2 + 32,
            i2 + 33,
            i2 + 48,
            i2 + 49,
            i2 + 64,
            i2 + 65,
            i2 + 80,
            i2 + 81,
            i2 + 96,
            i2 + 97,
            i2 + 112,
            i2 + 113,
        );
    }
}

pub(crate) fn register_bc_argon2_bytes_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let generator_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator";
    let block_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block";
    let block_desc = "Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;";
    let fill_block_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FillBlock";
    let fixed_pool_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool";

    r.register(
        generator_class,
        "generateBytes",
        "([B[BII)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let password_arr = obj_arg(args, 1)?;
            let out_arr = obj_arg(args, 2)?;
            let out_off = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => return Err(bc_argon2_bad_argument("invalid output offset")),
            };
            let out_len = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => return Err(bc_argon2_bad_argument("invalid output length")),
            };

            if out_len < 4 {
                return Err(bc_argon2_bad_state("output length less than 4"));
            }
            if out_off < 0
                || out_len < 0
                || (out_off as usize)
                    .checked_add(out_len as usize)
                    .map_or(true, |end| end > ctx.array_length(out_arr))
            {
                return Err(RuntimeError::aioobe_index_only(out_off).into());
            }

            let params = match ctx.get_field_by_name(this, "parameters") {
                Value::Object(Some(o)) => o,
                _ => return Err(bc_argon2_bad_state("Argon2BytesGenerator not initialized")),
            };

            let password = bc_argon2_read_byte_array(ctx, password_arr);
            let salt =
                bc_argon2_read_optional_byte_array(ctx, ctx.get_field_by_name(params, "salt"));
            let secret =
                bc_argon2_read_optional_byte_array(ctx, ctx.get_field_by_name(params, "secret"));
            let additional = bc_argon2_read_optional_byte_array(
                ctx,
                ctx.get_field_by_name(params, "additional"),
            );

            let iterations = bc_argon2_read_int_field(ctx, params, "iterations")?;
            let memory = bc_argon2_read_int_field(ctx, params, "memory")?;
            let lanes = bc_argon2_read_int_field(ctx, params, "lanes")?;
            let version = bc_argon2_read_int_field(ctx, params, "version")?;
            let argon_type = bc_argon2_read_int_field(ctx, params, "type")?;

            let algorithm = match argon_type {
                0 => argon2::Algorithm::Argon2d,
                1 => argon2::Algorithm::Argon2i,
                2 => argon2::Algorithm::Argon2id,
                _ => return Err(bc_argon2_bad_state("unknown Argon2 type")),
            };
            let version = match version {
                0x10 => argon2::Version::V0x10,
                0x13 => argon2::Version::V0x13,
                _ => return Err(bc_argon2_bad_state("unknown Argon2 version")),
            };

            let mut params_builder = argon2::ParamsBuilder::new();
            params_builder
                .m_cost(memory as u32)
                .t_cost(iterations as u32)
                .p_cost(lanes as u32)
                .output_len(out_len as usize);
            if !additional.is_empty() {
                let ad = argon2::AssociatedData::new(&additional).map_err(|e| {
                    bc_argon2_bad_argument(&format!("invalid Argon2 associated data: {e}"))
                })?;
                params_builder.data(ad);
            }
            let native_params = params_builder
                .build()
                .map_err(|e| bc_argon2_bad_argument(&format!("invalid Argon2 parameters: {e}")))?;
            let configured_pool = match ctx.get_field_by_name(params, "blockPool") {
                Value::Object(Some(pool)) => Some(pool),
                _ => None,
            };
            let out_pin = configured_pool.map(|_| (ctx.pin_native_root(out_arr), out_arr));
            if let Some(pool) = configured_pool {
                if let Err(err) =
                    bc_argon2_exercise_block_pool(ctx, pool, native_params.block_count())
                {
                    if let Some((pin, _)) = out_pin {
                        ctx.unpin_native_roots(pin);
                    }
                    return Err(err);
                }
            }
            let out_arr = match out_pin {
                Some((pin, original)) => ctx.read_native_pin(pin, original),
                None => out_arr,
            };
            let native_argon2 = if secret.is_empty() {
                argon2::Argon2::new(algorithm, version, native_params)
            } else {
                argon2::Argon2::new_with_secret(&secret, algorithm, version, native_params)
                    .map_err(|e| bc_argon2_bad_argument(&format!("invalid Argon2 secret: {e}")))?
            };

            let mut derived = vec![0u8; out_len as usize];
            native_argon2
                .hash_password_into(&password, &salt, &mut derived)
                .map_err(|e| bc_argon2_bad_argument(&format!("Argon2 generation failed: {e}")))?;
            if !ctx.write_byte_array_from(out_arr, out_off as usize, &derived) {
                return Err(RuntimeError::aioobe_index_only(out_off).into());
            }
            if let Some((pin, _)) = out_pin {
                ctx.unpin_native_roots(pin);
            }
            Ok(Some(Value::Int(out_len)))
        },
    );

    r.register(block_class, "fromBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let input = obj_arg(args, 1)?;
        if ctx.array_length(input) < 1024 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "input shorter than blocksize".into(),
            }
            .into());
        }
        let mut bytes = [0u8; 1024];
        ctx.read_byte_array_into(input, 0, &mut bytes);
        let mut words = [0u64; BC_ARGON2_BLOCK_QWORDS];
        for (i, word) in words.iter_mut().enumerate() {
            let off = i * 8;
            *word = u64::from_le_bytes([
                bytes[off],
                bytes[off + 1],
                bytes[off + 2],
                bytes[off + 3],
                bytes[off + 4],
                bytes[off + 5],
                bytes[off + 6],
                bytes[off + 7],
            ]);
        }
        bc_argon2_write_block_words(ctx, this, &words)?;
        Ok(None)
    });

    r.register(block_class, "toBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let output = obj_arg(args, 1)?;
        if ctx.array_length(output) < 1024 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "output shorter than blocksize".into(),
            }
            .into());
        }
        let words = bc_argon2_read_block_words(ctx, this)?;
        let mut bytes = [0u8; 1024];
        for (i, &word) in words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        ctx.write_byte_array_from(output, 0, &bytes);
        Ok(None)
    });

    r.register(
        block_class,
        "copyBlock",
        &format!("({block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let words = bc_argon2_read_block_words(ctx, other)?;
            bc_argon2_write_block_words(ctx, this, &words)?;
            Ok(None)
        },
    );

    r.register(
        block_class,
        "xor",
        &format!("({block_desc}{block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let b2 = obj_arg(args, 2)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            let w2 = bc_argon2_read_block_words(ctx, b2)?;
            let mut out = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] = w1[i] ^ w2[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    r.register(
        block_class,
        "xorWith",
        &format!("({block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let mut out = bc_argon2_read_block_words(ctx, this)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] ^= w1[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    r.register(
        block_class,
        "xorWith",
        &format!("({block_desc}{block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let b2 = obj_arg(args, 2)?;
            let mut out = bc_argon2_read_block_words(ctx, this)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            let w2 = bc_argon2_read_block_words(ctx, b2)?;
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] ^= w1[i] ^ w2[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    r.register(
        block_class,
        "clear",
        &format!("(){block_desc}"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            bc_argon2_clear_block(ctx, this)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(
        fixed_pool_class,
        "allocate",
        &format!("(){block_desc}"),
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let block = bc_argon2_alloc_block(ctx)?;
            bc_argon2_clear_block(ctx, block)?;
            Ok(Some(Value::Object(Some(block))))
        },
    );

    r.register(
        fixed_pool_class,
        "deallocate",
        &format!("({block_desc})V"),
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let block = obj_arg(args, 1)?;
            bc_argon2_clear_block(ctx, block)?;
            Ok(None)
        },
    );

    r.register(fill_block_class, "applyBlake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
        let mut z = bc_argon2_read_block_words(ctx, z_block)?;
        bc_argon2_apply_blake_words(&mut z);
        bc_argon2_write_block_words(ctx, z_block, &z)?;
        Ok(None)
    });

    r.register(
        fill_block_class,
        "fillBlock",
        &format!("({block_desc}{block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let y_block = obj_arg(args, 1)?;
            let current_block = obj_arg(args, 2)?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut z = y;
            bc_argon2_apply_blake_words(&mut z);
            let mut current = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] = y[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    r.register(
        fill_block_class,
        "fillBlock",
        &format!("({block_desc}{block_desc}{block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let x_block = obj_arg(args, 1)?;
            let y_block = obj_arg(args, 2)?;
            let current_block = obj_arg(args, 3)?;
            let r_block = bc_argon2_fillblock_block_field(ctx, this, "R")?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let x = bc_argon2_read_block_words(ctx, x_block)?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut r_words = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                r_words[i] = x[i] ^ y[i];
            }
            let mut z = r_words;
            bc_argon2_apply_blake_words(&mut z);
            let mut current = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] = r_words[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, r_block, &r_words)?;
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    r.register(
        fill_block_class,
        "fillBlockWithXor",
        &format!("({block_desc}{block_desc}{block_desc})V"),
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let x_block = obj_arg(args, 1)?;
            let y_block = obj_arg(args, 2)?;
            let current_block = obj_arg(args, 3)?;
            let r_block = bc_argon2_fillblock_block_field(ctx, this, "R")?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let x = bc_argon2_read_block_words(ctx, x_block)?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut current = bc_argon2_read_block_words(ctx, current_block)?;
            let mut r_words = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                r_words[i] = x[i] ^ y[i];
            }
            let mut z = r_words;
            bc_argon2_apply_blake_words(&mut z);
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] ^= r_words[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, r_block, &r_words)?;
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    r.register(
        generator_class,
        "roundFunction",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;IIIIIIIIIIIIIIII)V",
        |ctx, args| {
            let block = obj_arg(args, 0)?;
            let mut words = bc_argon2_read_block_words(ctx, block)?;

            let v0 = bc_argon2_round_index(args, 1)?;
            let v1 = bc_argon2_round_index(args, 2)?;
            let v2 = bc_argon2_round_index(args, 3)?;
            let v3 = bc_argon2_round_index(args, 4)?;
            let v4 = bc_argon2_round_index(args, 5)?;
            let v5 = bc_argon2_round_index(args, 6)?;
            let v6 = bc_argon2_round_index(args, 7)?;
            let v7 = bc_argon2_round_index(args, 8)?;
            let v8 = bc_argon2_round_index(args, 9)?;
            let v9 = bc_argon2_round_index(args, 10)?;
            let v10 = bc_argon2_round_index(args, 11)?;
            let v11 = bc_argon2_round_index(args, 12)?;
            let v12 = bc_argon2_round_index(args, 13)?;
            let v13 = bc_argon2_round_index(args, 14)?;
            let v14 = bc_argon2_round_index(args, 15)?;
            let v15 = bc_argon2_round_index(args, 16)?;

            bc_argon2_round_function_words(
                &mut words, v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15,
            );
            bc_argon2_write_block_words(ctx, block, &words)?;
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn bc_pkcs12_adjust(a: &mut [u8], a_off: usize, b: &[u8]) {
    let mut x = u16::from(b[b.len() - 1]) + u16::from(a[a_off + b.len() - 1]) + 1;
    a[a_off + b.len() - 1] = x as u8;
    x >>= 8;

    for i in (0..b.len() - 1).rev() {
        x += u16::from(b[i]) + u16::from(a[a_off + i]);
        a[a_off + i] = x as u8;
        x >>= 8;
    }
}

pub(crate) fn bc_pkcs12_repeat_to_v(input: &[u8], v: usize) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }
    let len = v * input.len().div_ceil(v);
    let mut out = vec![0u8; len];
    for i in 0..len {
        out[i] = input[i % input.len()];
    }
    out
}

pub(crate) fn bc_pkcs12_derive_sha1(
    password: &[u8],
    salt: &[u8],
    iteration_count: i32,
    id_byte: u8,
    n: usize,
) -> Vec<u8> {
    const U: usize = 20;
    const V: usize = 64;

    let d = vec![id_byte; V];
    let s = bc_pkcs12_repeat_to_v(salt, V);
    let p = bc_pkcs12_repeat_to_v(password, V);
    let mut i_buf = Vec::with_capacity(s.len() + p.len());
    i_buf.extend_from_slice(&s);
    i_buf.extend_from_slice(&p);

    let mut d_key = vec![0u8; n];
    let c = n.div_ceil(U);
    for i in 1..=c {
        let mut input = Vec::with_capacity(d.len() + i_buf.len());
        input.extend_from_slice(&d);
        input.extend_from_slice(&i_buf);
        let mut a = crate::real_sha1(&input);
        for _ in 1..iteration_count {
            a = crate::real_sha1(&a);
        }

        let mut b = vec![0u8; V];
        for j in 0..V {
            b[j] = a[j % a.len()];
        }
        for j in 0..(i_buf.len() / V) {
            bc_pkcs12_adjust(&mut i_buf, j * V, &b);
        }

        let d_off = (i - 1) * U;
        let copy_len = if i == c { n - d_off } else { U };
        d_key[d_off..d_off + copy_len].copy_from_slice(&a[..copy_len]);
    }
    d_key
}

pub(crate) fn bc_pkcs12_generator_bytes(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Vec<u8> {
    match ctx.get_field_by_name(this, name) {
        Value::Object(Some(arr)) => {
            let len = ctx.array_length(arr);
            let mut out = vec![0u8; len];
            let n = ctx.read_byte_array_into(arr, 0, &mut out);
            out.truncate(n);
            out
        }
        _ => Vec::new(),
    }
}

/// Is this `PKCS12ParametersGenerator` the SHA-1 one the native KDF computes?
///
/// PKCS#12's KDF is parameterised by a digest: `new PKCS12ParametersGenerator(
/// new SHA1Digest())` is the common case and the only one
/// `bc_pkcs12_derive_sha1` implements. Every other digest — SHA-256, GOST3411 —
/// must run BouncyCastle's OWN bytecode, exactly as the `PKCS5S2` sibling does
/// for a PRF this VM has no arm for. Raising instead turned three bc-java
/// `PfxPduTest` cases (`testGOST1`, `testGOST2`, `testCreateAES256andSHA256`)
/// into hard errors from inside the MAC-calculator builder.
fn bc_pkcs12_is_sha1(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Value::Object(Some(digest)) = ctx.get_field_by_name(this, "digest") else {
        return false;
    };
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(digest))
            .as_deref(),
        Some("org/bouncycastle/crypto/digests/SHA1Digest")
    )
}

pub(crate) fn bc_pkcs12_require_sha1(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let digest = match ctx.get_field_by_name(this, "digest") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "PKCS12ParametersGenerator: missing digest".into(),
            }
            .into())
        }
    };
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(digest))
        .as_deref()
    {
        Some("org/bouncycastle/crypto/digests/SHA1Digest") => Ok(()),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator native supports SHA1Digest".into(),
        }
        .into()),
    }
}

pub(crate) fn bc_pkcs12_read_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(Vec<u8>, Vec<u8>, i32), MethodCallFailed> {
    bc_pkcs12_require_sha1(ctx, this)?;
    let password = bc_pkcs12_generator_bytes(ctx, this, "password");
    let salt = bc_pkcs12_generator_bytes(ctx, this, "salt");
    // SECURITY — the sibling `bc_pkcs5s2_read_state` below already refuses a
    // non-positive iteration count; this one accepted it. That mattered because
    // `get_field_by_name` is not descriptor-aware and cannot distinguish "the
    // generator was never `init`-ed" from "iterationCount is genuinely 0": an
    // unwritten slot answers `Value::Int(0)`, which the first arm ACCEPTED, so
    // the `_ => 0` fallback was not even the path that produced the zero. A
    // PKCS#12 KDF run with zero iterations degenerates the derived key. Read by
    // resolved index and refuse. See `docs/feature-designs/by-name-field-reads.md`.
    let iteration_count = match crate::field_read::int_field_strict(ctx, this, "iterationCount") {
        Some(v) if v > 0 => v,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "iteration count must be at least 1.".into(),
            }
            .into())
        }
    };
    Ok((password, salt, iteration_count))
}

pub(crate) fn bc_pkcs5s2_read_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(Vec<u8>, Vec<u8>, u32), MethodCallFailed> {
    let password = bc_pkcs12_generator_bytes(ctx, this, "password");
    let salt = bc_pkcs12_generator_bytes(ctx, this, "salt");
    let iteration_count = match ctx.get_field_by_name(this, "iterationCount") {
        Value::Int(v) if v > 0 => v as u32,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "iteration count must be at least 1.".into(),
            }
            .into())
        }
    };
    Ok((password, salt, iteration_count))
}

pub(crate) fn bc_pkcs12_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    if !bytes.is_empty() {
        ctx.write_byte_array_from(arr, 0, bytes);
    }
    arr
}

pub(crate) fn bc_pkcs12_key_parameter(
    ctx: &mut dyn NativeContext,
    key: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let key_arr = bc_pkcs12_byte_array(ctx, key);
    let key_pin = ctx.pin_native_root(key_arr);
    let key_arr = ctx.read_native_pin(key_pin, key_arr);
    let result = ctx.new_object_initialized(
        "org/bouncycastle/crypto/params/KeyParameter",
        "([BII)V",
        &[
            Value::Object(Some(key_arr)),
            Value::Int(0),
            Value::Int(key.len() as i32),
        ],
    );
    ctx.unpin_native_roots(key_pin);
    match result? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator: KeyParameter allocation failed".into(),
        }
        .into()),
    }
}

pub(crate) fn bc_pkcs12_parameters_with_iv(
    ctx: &mut dyn NativeContext,
    key: &[u8],
    iv: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let key_param = bc_pkcs12_key_parameter(ctx, key)?;
    let key_pin = ctx.pin_native_root(key_param);
    let iv_arr = bc_pkcs12_byte_array(ctx, iv);
    let iv_pin = ctx.pin_native_root(iv_arr);
    let key_param = ctx.read_native_pin(key_pin, key_param);
    let iv_arr = ctx.read_native_pin(iv_pin, iv_arr);
    let result = ctx.new_object_initialized(
        "org/bouncycastle/crypto/params/ParametersWithIV",
        "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V",
        &[
            Value::Object(Some(key_param)),
            Value::Object(Some(iv_arr)),
            Value::Int(0),
            Value::Int(iv.len() as i32),
        ],
    );
    ctx.unpin_native_roots(key_pin);
    match result? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator: ParametersWithIV allocation failed".into(),
        }
        .into()),
    }
}

pub(crate) fn register_bc_pkcs12_parameters_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/crypto/generators/PKCS12ParametersGenerator";
    r.register(
        cls,
        "generateDerivedParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 1, key_size);
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cls,
        "generateDerivedParameters",
        "(II)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let iv_size = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(II)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[
                        args.get(1).copied().unwrap_or(Value::Int(0)),
                        args.get(2).copied().unwrap_or(Value::Int(0)),
                    ],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 1, key_size);
            let iv = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 2, iv_size);
            let obj = bc_pkcs12_parameters_with_iv(ctx, &key, &iv)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cls,
        "generateDerivedMacParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedMacParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 3, key_size);
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.set_category(__prev_cat);
}

/// Which PRF does this `PKCS5S2ParametersGenerator` actually carry?
///
/// PKCS#5 v2.0 is parameterised by an HMAC: `new PKCS5S2ParametersGenerator()`
/// is HMAC-SHA1, but `new PKCS5S2ParametersGenerator(new SHA256Digest())` — the
/// form BouncyCastle's own `PBE$Util.makePBEGenerator` uses for every non-SHA1
/// PRF — is not. The three `generateDerived*` intrinsics below used to pass a
/// hardcoded `1` (SHA-1) to `pbkdf2_derive_for` regardless, so **every**
/// non-SHA1 PBKDF2 through BouncyCastle silently derived the wrong key.
///
/// Measured 2026-08-13 against HotSpot 25 with the same jars: netty's
/// `SslContextBuilderTest`/`JdkSsl*ContextTest` `testPkcs8Des3EncryptedRsa`
/// reads `rsa_pkcs8_des3_encrypted.key`, PBES2 with PBKDF2-HMAC-**SHA256** and
/// DESede-CBC. `PBE$Util.makePBEMacParameters(spec, PKCS5S2_UTF8, SHA256, 192)`
/// returned `992951E4…` (the SHA-1 answer) instead of `9559B2B3…`, the DESede
/// decrypt then failed `BadPaddingException: pad block corrupted`, netty fell
/// back to the JDK PBES2 parser, and the test surfaced the JDK's own
/// `IOException: PBE parameter parsing error: expecting the object identifier
/// for AES cipher` — an error message three layers away from the defect.
///
/// Reads the generator's `hMac` field and asks it its own name
/// (`HMac.getAlgorithmName()` is `"<digest>/HMAC"`). Returns `None` for any
/// digest this VM's `pbkdf2_derive_for` does not implement (GOST3411, SM3,
/// SHA3-*, RIPEMD160, Whirlpool, …) so the caller can fall back to
/// BouncyCastle's own bytecode instead of substituting a PRF of our choosing —
/// substituting is exactly what produced the defect above.
fn bc_pkcs5s2_prf_code(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let hmac = match ctx.get_field_by_name(this, "hMac") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let name = match ctx.invoke_virtual(hmac, "getAlgorithmName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => return None,
    };
    // "SHA-256/HMAC" -> "SHA256"; also tolerates BC's older "SHA-256" spellings.
    let norm: String = name
        .split('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match norm.as_str() {
        "SHA1" => Some(1),
        "SHA224" => Some(224),
        "SHA256" => Some(256),
        "SHA384" => Some(384),
        "SHA512" => Some(512),
        _ => None,
    }
}

pub(crate) fn register_bc_pkcs5s2_parameters_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);

    let cls = "org/bouncycastle/crypto/generators/PKCS5S2ParametersGenerator";
    r.register(
        cls,
        "generateDerivedParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let key = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size,
            );
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cls,
        "generateDerivedParameters",
        "(II)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let iv_size = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(II)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[
                        args.get(1).copied().unwrap_or(Value::Int(0)),
                        args.get(2).copied().unwrap_or(Value::Int(0)),
                    ],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let derived = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size + iv_size,
            );
            let obj = bc_pkcs12_parameters_with_iv(
                ctx,
                &derived[..key_size],
                &derived[key_size..key_size + iv_size],
            )?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        cls,
        "generateDerivedMacParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedMacParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let key = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size,
            );
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.set_category(__prev_cat);
}

pub(crate) fn bc_bcrypt_illegal(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub(crate) fn bc_bcrypt_byte_array_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    index: usize,
    null_message: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Ok(*o),
        Some(Value::Object(None)) => Err(bc_bcrypt_illegal(null_message)),
        _ => Err(RuntimeError::IllegalStateException {
            message: "BCrypt.generate: malformed byte[] argument".into(),
        }
        .into()),
    }
    .map(|arr| {
        let _ = ctx.array_length(arr);
        arr
    })
}

pub(crate) fn bc_bcrypt_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub(crate) fn bc_bcrypt_read_static_i32_array(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_name: &str,
) -> Result<Vec<i32>, MethodCallFailed> {
    let field = ctx
        .static_field_index_by_name(class_id, field_name)
        .ok_or_else(|| RuntimeError::IllegalStateException {
            message: format!("BCrypt native: missing static field {field_name}"),
        })?;
    let arr = match ctx.get_static_field(class_id, field) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("BCrypt native: malformed static field {field_name}"),
            }
            .into())
        }
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push(v),
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("BCrypt native: malformed int[] field {field_name}"),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub(crate) fn bc_bcrypt_f(s: &[i32], x: i32) -> i32 {
    let x = x as u32;
    let a = s[(x >> 24) as usize];
    let b = s[256 + ((x >> 16) & 0xff) as usize];
    let c = s[512 + ((x >> 8) & 0xff) as usize];
    let d = s[768 + (x & 0xff) as usize];
    (a.wrapping_add(b) ^ c).wrapping_add(d)
}

pub(crate) fn bc_bcrypt_encipher(p: &[i32], s: &[i32], mut xl: i32, mut xr: i32) -> (i32, i32) {
    xl ^= p[0];
    let mut i = 1usize;
    while i < 16 {
        xr ^= bc_bcrypt_f(s, xl) ^ p[i];
        xl ^= bc_bcrypt_f(s, xr) ^ p[i + 1];
        i += 2;
    }
    xr ^= p[17];
    (xr, xl)
}

pub(crate) fn bc_bcrypt_process_p(p: &mut [i32], s: &[i32], mut xl: i32, mut xr: i32) {
    let mut idx = 0usize;
    while idx < p.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        p[idx] = left;
        p[idx + 1] = right;
        xr = right;
        xl = left;
        idx += 2;
    }
}

pub(crate) fn bc_bcrypt_process_s(p: &[i32], s: &mut [i32], mut xl: i32, mut xr: i32) {
    let mut idx = 0usize;
    while idx < s.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        s[idx] = left;
        s[idx + 1] = right;
        xr = right;
        xl = left;
        idx += 2;
    }
}

pub(crate) fn bc_bcrypt_process_p_with_salt(
    p: &mut [i32],
    s: &[i32],
    salt: &[i32; 4],
    iv1: i32,
    iv2: i32,
) {
    let mut xl = iv1 ^ salt[0];
    let mut xr = iv2 ^ salt[1];
    let mut idx = 0usize;
    while idx < p.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        p[idx] = left;
        p[idx + 1] = right;

        let yl = salt[2] ^ left;
        let yr = salt[3] ^ right;
        if idx + 2 >= p.len() {
            break;
        }

        let (left2, right2) = bc_bcrypt_encipher(p, s, yl, yr);
        p[idx + 2] = left2;
        p[idx + 3] = right2;
        xl = salt[0] ^ left2;
        xr = salt[1] ^ right2;
        idx += 4;
    }
}

pub(crate) fn bc_bcrypt_process_s_with_salt(
    p: &[i32],
    s: &mut [i32],
    salt: &[i32; 4],
    iv1: i32,
    iv2: i32,
) {
    let mut xl = iv1 ^ salt[0];
    let mut xr = iv2 ^ salt[1];
    let mut idx = 0usize;
    while idx < s.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        s[idx] = left;
        s[idx + 1] = right;

        let yl = salt[2] ^ left;
        let yr = salt[3] ^ right;
        if idx + 2 >= s.len() {
            break;
        }

        let (left2, right2) = bc_bcrypt_encipher(p, s, yl, yr);
        s[idx + 2] = left2;
        s[idx + 3] = right2;
        xl = salt[0] ^ left2;
        xr = salt[1] ^ right2;
        idx += 4;
    }
}

pub(crate) fn bc_bcrypt_cyclic_xor_key(p: &mut [i32], key: &[u8]) {
    let mut key_index = 0usize;
    for slot in p.iter_mut() {
        let mut data = 0i32;
        for _ in 0..4 {
            data = (data << 8) | i32::from(key[key_index] & 0xff);
            key_index += 1;
            if key_index >= key.len() {
                key_index = 0;
            }
        }
        *slot ^= data;
    }
}

pub(crate) fn bc_bcrypt_be_i32(bytes: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

pub(crate) fn bc_bcrypt_ints_to_be_bytes(ints: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ints.len() * 4);
    for &v in ints {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Execute the common BCrypt key schedule against a library's published P/S
/// constants. Both Bouncy Castle and Spring Security expose the same standard
/// Blowfish tables, but they use different class/field names and otherwise
/// leave this deliberately expensive work in Java bytecode.
pub(crate) fn bc_bcrypt_generate_raw_with_constants(
    ctx: &mut dyn NativeContext,
    password: &[u8],
    salt: &[u8],
    cost: i32,
    class_name: &str,
    p_fields: &[&str],
    s_fields: &[&str],
) -> Result<Vec<u8>, MethodCallFailed> {
    let class_id = ctx.ensure_class_initialized(class_name)?;
    let mut p = Vec::new();
    for field in p_fields {
        p.extend_from_slice(&bc_bcrypt_read_static_i32_array(ctx, class_id, field)?);
    }
    let mut s = Vec::with_capacity(1024);
    for field in s_fields {
        s.extend_from_slice(&bc_bcrypt_read_static_i32_array(ctx, class_id, field)?);
    }
    if p.len() != 18 || s.len() != 1024 {
        return Err(RuntimeError::IllegalStateException {
            message: "BCrypt native: malformed P/S constants".into(),
        }
        .into());
    }

    let mut psw = if password.is_empty() {
        vec![0u8; 4]
    } else {
        password.to_vec()
    };
    let salt32 = [
        bc_bcrypt_be_i32(salt, 0),
        bc_bcrypt_be_i32(salt, 4),
        bc_bcrypt_be_i32(salt, 8),
        bc_bcrypt_be_i32(salt, 12),
    ];
    let salt32_swapped = [salt32[2], salt32[3], salt32[0], salt32[1]];

    bc_bcrypt_cyclic_xor_key(&mut p, &psw);
    bc_bcrypt_process_p_with_salt(&mut p, &s, &salt32, 0, 0);
    let p_tail0 = p[p.len() - 2];
    let p_tail1 = p[p.len() - 1];
    bc_bcrypt_process_s_with_salt(&p, &mut s, &salt32_swapped, p_tail0, p_tail1);

    let rounds = 1u32.checked_shl(cost as u32).unwrap_or(0);
    for _ in 0..rounds {
        bc_bcrypt_cyclic_xor_key(&mut p, &psw);
        bc_bcrypt_process_p(&mut p, &s, 0, 0);
        let p_tail0 = p[p.len() - 2];
        let p_tail1 = p[p.len() - 1];
        bc_bcrypt_process_s(&p, &mut s, p_tail0, p_tail1);

        bc_bcrypt_cyclic_xor_key(&mut p, salt);
        bc_bcrypt_process_p(&mut p, &s, 0, 0);
        let p_tail0 = p[p.len() - 2];
        let p_tail1 = p[p.len() - 1];
        bc_bcrypt_process_s(&p, &mut s, p_tail0, p_tail1);
    }

    let mut text = [
        0x4f72_7068i32,
        0x6561_6e42i32,
        0x6568_6f6ci32,
        0x6465_7253i32,
        0x6372_7944i32,
        0x6f75_6274i32,
    ];
    for _ in 0..64 {
        for j in (0..6).step_by(2) {
            let (left, right) = bc_bcrypt_encipher(&p, &s, text[j], text[j + 1]);
            text[j] = left;
            text[j + 1] = right;
        }
    }
    psw.fill(0);
    Ok(bc_bcrypt_ints_to_be_bytes(&text))
}

pub(crate) fn bc_bcrypt_generate_raw(
    ctx: &mut dyn NativeContext,
    password: &[u8],
    salt: &[u8],
    cost: i32,
) -> Result<Vec<u8>, MethodCallFailed> {
    bc_bcrypt_generate_raw_with_constants(
        ctx,
        password,
        salt,
        cost,
        "org/bouncycastle/crypto/generators/BCrypt",
        &["KP"],
        &["KS0", "KS1", "KS2", "KS3"],
    )
}

pub(crate) fn bc_bcrypt_generate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pw_arr = bc_bcrypt_byte_array_arg(ctx, args, 0, "pwInput and salt are required")?;
    let salt_arr = bc_bcrypt_byte_array_arg(ctx, args, 1, "pwInput and salt are required")?;
    let cost = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if ctx.array_length(salt_arr) != 16 {
        return Err(bc_bcrypt_illegal("BCrypt salt must be 128 bits"));
    }
    if ctx.array_length(pw_arr) > 72 {
        return Err(bc_bcrypt_illegal("BCrypt password must be <= 72 bytes"));
    }
    if !(4..=31).contains(&cost) {
        return Err(bc_bcrypt_illegal("BCrypt cost must be from 4..31"));
    }
    let password = bc_bcrypt_read_byte_array(ctx, pw_arr);
    let salt = bc_bcrypt_read_byte_array(ctx, salt_arr);
    let hash = bc_bcrypt_generate_raw(ctx, &password, &salt, cost)?;
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hash.len());
    ctx.write_byte_array_from(out, 0, &hash);
    Ok(Some(Value::Object(Some(out))))
}

/// Spring Security's BCrypt implementation spends virtually all of its time
/// in `crypt_raw` repeatedly invoking its Java `encipher` loop. At the normal
/// cost of 10, that is fast on HotSpot but takes minutes in the interpreter,
/// turning ordinary password assertions into suite timeouts. Keep Spring's
/// surrounding bytecode responsible for salt parsing, revision handling,
/// output encoding, and comparison; replace only the standard key schedule.
///
/// `sign_ext_bug` is only used for the historical `$2x$` compatibility mode.
/// Spring's supported `$2a$`, `$2b$`, and `$2y$` paths pass false and share the
/// standard BCrypt schedule used below. Do not apply this intrinsic to `$2x$`:
/// that obsolete compatibility variant must retain Spring's bytecode semantics.
pub(crate) fn spring_security_bcrypt_crypt_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let password_arr = bc_bcrypt_byte_array_arg(ctx, args, 1, "Bad password")?;
    let salt_arr = bc_bcrypt_byte_array_arg(ctx, args, 2, "Bad salt length")?;
    let cost = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let sign_ext_bug = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    if sign_ext_bug {
        return ctx.invoke_special_bytecode_only(
            "org/springframework/security/crypto/bcrypt/BCrypt",
            "crypt_raw",
            "([B[BIZIZ)[B",
            args,
        );
    }
    if ctx.array_length(salt_arr) != 16 {
        return Err(bc_bcrypt_illegal("Bad salt length"));
    }
    if !(4..=31).contains(&cost) {
        return Err(bc_bcrypt_illegal("Bad number of rounds"));
    }
    let password = bc_bcrypt_read_byte_array(ctx, password_arr);
    let salt = bc_bcrypt_read_byte_array(ctx, salt_arr);
    let hash = bc_bcrypt_generate_raw_with_constants(
        ctx,
        &password,
        &salt,
        cost,
        "org/springframework/security/crypto/bcrypt/BCrypt",
        &["P_orig"],
        &["S_orig"],
    )?;
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hash.len());
    ctx.write_byte_array_from(out, 0, &hash);
    Ok(Some(Value::Object(Some(out))))
}

pub(crate) fn register_bc_bcrypt_generator(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        "org/bouncycastle/crypto/generators/BCrypt",
        "generate",
        "([B[BI)[B",
        bc_bcrypt_generate,
    );
    r.register(
        "org/springframework/security/crypto/bcrypt/BCrypt",
        "crypt_raw",
        "([B[BIZIZ)[B",
        spring_security_bcrypt_crypt_raw,
    );
    r.set_category(__prev_cat);
}
