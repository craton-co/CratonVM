#[allow(unused_imports)]
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
pub const BC_SMALL_FACTOR_GROUPS: [&[u32]; 10] = [
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
pub fn bc_has_any_small_factors(mag: &[u32], negative: bool) -> bool {
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
pub const BC_ODD_SMALL_PRIMES: [u32; 131] = [
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
pub fn mag_mod_u32(mag: &[u32], p: u32) -> u32 {
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
pub fn bc_util_has_any_small_factors(mag: &[u32]) -> bool {
    // x even? (low limb's bit 0; empty magnitude == 0 == even)
    if mag.first().copied().unwrap_or(0) & 1 == 0 {
        return true;
    }
    BC_ODD_SMALL_PRIMES
        .iter()
        .any(|&p| mag_mod_u32(mag, p) == 0)
}

pub fn bc_bigint_small(v: u32) -> crate::bigint::BigInt {
    if v == 0 {
        crate::bigint::BigInt::zero()
    } else {
        crate::bigint::BigInt::from_le_words(false, vec![v])
    }
}

pub fn bc_primes_candidate_arg(
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

pub fn bc_mr_probable_prime_to_base(
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

pub fn bc_big_integers_mod_odd_inverse(
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

pub fn bc_long_array_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_long_array_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, "m_ints") {
        Value::Object(Some(arr)) => Ok(arr),
        _ => Err(bc_long_array_bad_state("LongArray: malformed m_ints")),
    }
}

pub fn bc_read_long_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u64> {
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

pub fn bc_read_longarray_value(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<Vec<u64>, MethodCallFailed> {
    Ok(bc_read_long_array(ctx, bc_long_array_field(ctx, obj)?))
}

pub fn bc_read_i32_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i32> {
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

pub fn bc_longarray_ks(
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

pub fn bc_trim_poly(x: &mut Vec<u64>) {
    while x.last().copied() == Some(0) {
        x.pop();
    }
}

pub fn bc_poly_degree(x: &[u64]) -> usize {
    for (i, &word) in x.iter().enumerate().rev() {
        if word != 0 {
            return i * 64 + (64 - word.leading_zeros() as usize);
        }
    }
    0
}

pub fn bc_poly_test_bit(x: &[u64], bit: usize) -> bool {
    x.get(bit >> 6)
        .map(|word| (word & (1u64 << (bit & 63))) != 0)
        .unwrap_or(false)
}

pub fn bc_poly_flip_bit(x: &mut Vec<u64>, bit: usize) {
    let word = bit >> 6;
    if word >= x.len() {
        x.resize(word + 1, 0);
    }
    x[word] ^= 1u64 << (bit & 63);
}

pub fn bc_poly_xor_shift(dst: &mut Vec<u64>, src: &[u64], shift: usize) {
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

pub fn bc_poly_mask_to_m(x: &mut Vec<u64>, m: usize) {
    let len = (m + 63) >> 6;
    x.truncate(len);
    if m & 63 != 0 {
        if x.len() == len {
            x[len - 1] &= (1u64 << (m & 63)) - 1;
        }
    }
    bc_trim_poly(x);
}

pub fn bc_poly_modulus(m: usize, ks: &[usize]) -> Vec<u64> {
    let mut f = Vec::new();
    bc_poly_flip_bit(&mut f, 0);
    bc_poly_flip_bit(&mut f, m);
    for &k in ks {
        bc_poly_flip_bit(&mut f, k);
    }
    f
}

pub fn bc_poly_reduce(mut x: Vec<u64>, m: usize, ks: &[usize]) -> Vec<u64> {
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

pub fn bc_poly_mul_raw(a: &[u64], b: &[u64]) -> Vec<u64> {
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

pub fn bc_poly_square_raw(a: &[u64]) -> Vec<u64> {
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

pub fn bc_poly_mod_square_n(mut a: Vec<u64>, n: i32, m: usize, ks: &[usize]) -> Vec<u64> {
    let rounds = n.max(0) as usize;
    for _ in 0..rounds {
        a = bc_poly_reduce(bc_poly_square_raw(&a), m, ks);
    }
    a
}

pub fn bc_poly_inverse(a: &[u64], m: usize, ks: &[usize]) -> Option<Vec<u64>> {
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
pub const LONG_ARRAY_MIN_WORDS: usize = 1;

pub fn bc_alloc_long_array(
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

pub fn bc_longarray_return(ctx: &mut dyn NativeContext, words: &[u64]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_alloc_long_array(ctx, words)?))))
}

pub fn bc_poly_add(mut a: Vec<u64>, b: &[u64]) -> Vec<u64> {
    bc_poly_xor_shift(&mut a, b, 0);
    bc_trim_poly(&mut a);
    a
}

pub fn bc_longarray_set_value(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    words: &[u64],
) -> Result<(), MethodCallFailed> {
    // gc-common w19-f: `this` is rooted across the array's allocation (which
    // can collect) and re-read after it; the store used to go through the
    // receiver's pre-allocation address.
    let this_pin = ctx.pin_native_root(this);
    // Same floor as `bc_alloc_long_array`; see `LONG_ARRAY_MIN_WORDS`.
    let arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Long,
        words.len().max(LONG_ARRAY_MIN_WORDS),
    );
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(word as i64));
    }
    ctx.set_field_by_name(this, "m_ints", Value::Object(Some(arr)));
    Ok(())
}

pub fn bc_longarray_this_or_other(
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

pub fn register_bc_long_array(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/math/ec/LongArray";

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(r, CLS, "reduce", "(I[I)V", |ctx, args| {
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    }

pub fn bc_f2m_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_f2m_ks_from_obj(
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

pub fn bc_f2m_read_this(
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

pub fn bc_f2m_arg_value(
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

pub fn bc_f2m_alloc(
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
            // gc-common w19-f: `ks` is read through its pin for the length too;
            // the raw `ks_obj` is from before both allocations above.
            let ks_obj = ctx.read_native_pin(base_pin, ks_obj);
            ctx.set_field_by_name(obj, "m", Value::Int(m));
            ctx.set_field_by_name(
                obj,
                "representation",
                Value::Int(if ctx.array_length(ks_obj) == 1 { 2 } else { 3 }),
            );
            ctx.set_field_by_name(obj, "ks", Value::Object(Some(ks_obj)));
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

pub fn bc_f2m_return(
    ctx: &mut dyn NativeContext,
    m: i32,
    ks_obj: ObjectRef,
    x: Vec<u64>,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_f2m_alloc(ctx, m, ks_obj, &x)?))))
}

pub fn register_bc_f2m_field_element(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/math/ec/ECFieldElement$F2m";

    bc_register_null_rerun!(
        r,
        CLS,
        "add",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, _, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            bc_f2m_return(ctx, m, ks_obj, bc_poly_add(x, &rhs))
        },
    );

    bc_register_null_rerun!(
        r,
        CLS,
        "subtract",
        "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, _, x) = bc_f2m_read_this(ctx, this)?;
            let rhs = bc_f2m_arg_value(ctx, args, 1)?;
            bc_f2m_return(ctx, m, ks_obj, bc_poly_add(x, &rhs))
        },
    );

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |_ctx, args| Ok(Some(Value::Object(Some(obj_arg(args, 0)?)))),
    );

    bc_register_null_rerun!(
        r,
        CLS,
        "square",
        "()Lorg/bouncycastle/math/ec/ECFieldElement;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (m, ks_obj, ks, x) = bc_f2m_read_this(ctx, this)?;
            let z = bc_poly_reduce(bc_poly_square_raw(&x), m as usize, &ks);
            bc_f2m_return(ctx, m, ks_obj, z)
        },
    );

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

}

pub struct BcF2mPointData {
    pub curve: ObjectRef,
    pub coord: i32,
    pub m: i32,
    pub ks_obj: ObjectRef,
    pub ks: Vec<usize>,
    pub x: Vec<u64>,
    pub y: Vec<u64>,
    pub z: Vec<u64>,
}

pub fn bc_f2m_add2(a: &[u64], b: &[u64]) -> Vec<u64> {
    bc_poly_add(a.to_vec(), b)
}

pub fn bc_f2m_add_many(first: &[u64], rest: &[&[u64]]) -> Vec<u64> {
    let mut out = first.to_vec();
    for part in rest {
        out = bc_poly_add(out, part);
    }
    out
}

pub fn bc_f2m_mul_mod(a: &[u64], b: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_reduce(bc_poly_mul_raw(a, b), m as usize, ks)
}

pub fn bc_f2m_square_mod(a: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_reduce(bc_poly_square_raw(a), m as usize, ks)
}

pub fn bc_f2m_square_plus_product(
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

pub fn bc_f2m_multiply_plus_product(
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

pub fn bc_f2m_inverse_mod(
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

pub fn bc_f2m_div_mod(
    a: &[u64],
    b: &[u64],
    m: i32,
    ks: &[usize],
) -> Result<Vec<u64>, MethodCallFailed> {
    let inv = bc_f2m_inverse_mod(b, m, ks)?;
    Ok(bc_f2m_mul_mod(a, &inv, m, ks))
}

pub fn bc_f2m_sqrt_mod(a: &[u64], m: i32, ks: &[usize]) -> Vec<u64> {
    bc_poly_mod_square_n(a.to_vec(), m.saturating_sub(1), m as usize, ks)
}

pub fn bc_f2m_add_one(mut a: Vec<u64>) -> Vec<u64> {
    bc_poly_flip_bit(&mut a, 0);
    bc_trim_poly(&mut a);
    a
}

pub fn bc_f2m_curve_coord(ctx: &dyn NativeContext, curve: ObjectRef) -> i32 {
    match ctx.get_field_by_name(curve, "coord") {
        Value::Int(coord) => coord,
        _ => 0,
    }
}

pub fn bc_f2m_curve_field_words(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
    name: &str,
) -> Result<(i32, ObjectRef, Vec<usize>, Vec<u64>), MethodCallFailed> {
    let field = bc_fp_obj_field(ctx, curve, name)?;
    bc_f2m_read_this(ctx, field)
}

pub fn bc_f2m_point_read(
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

pub fn bc_f2m_point_infinity(ctx: &dyn NativeContext, curve: ObjectRef) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_f2m_bad_state("ECPoint.F2m: curve has no infinity point")),
    }
}

pub fn bc_f2m_alloc_point(
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

pub fn bc_f2m_point_affine_double_data(
    ctx: &dyn NativeContext,
    p: &BcF2mPointData,
) -> Result<Option<(Vec<u64>, Vec<u64>, Vec<u64>)>, MethodCallFailed> {
    let l1 = bc_f2m_add2(&bc_f2m_div_mod(&p.y, &p.x, p.m, &p.ks)?, &p.x);
    let (_, _, _, curve_a) = bc_f2m_curve_field_words(ctx, p.curve, "a")?;
    let x3 = bc_f2m_add_many(&bc_f2m_square_mod(&l1, p.m, &p.ks), &[&l1, &curve_a]);
    let y3 = bc_f2m_square_plus_product(&p.x, &x3, &bc_f2m_add_one(l1.clone()), p.m, &p.ks);
    Ok(Some((x3, y3, vec![1])))
}

pub fn bc_f2m_point_homogeneous_double_data(
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

pub fn bc_f2m_point_lambda_double_data(
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

pub fn bc_f2m_point_double_data(
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

pub fn bc_f2m_point_affine_add_data(
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

pub fn bc_f2m_point_homogeneous_add_data(
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

pub fn bc_f2m_point_lambda_add_data(
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

pub fn bc_f2m_point_add_data(
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

pub fn bc_f2m_point_return_twice(
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

pub fn bc_f2m_point_return_add(
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

pub fn bc_f2m_point_return_twice_plus(
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

pub fn register_bc_f2m_point(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/math/ec/ECPoint$F2m";
    bc_register_null_rerun!(
        r,
        CLS,
        "add",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_add,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "twice",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_twice,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "twicePlus",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_f2m_point_return_twice_plus,
    );

    }

pub fn bc_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i8> {
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

pub fn bc_ec_point_virtual_obj(
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

pub fn bc_ecalg_impl_shamirs_trick_jsf(
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

pub fn register_bc_ec_algorithms(r: &mut NativeMethodRegistry) {
        
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/math/ec/ECAlgorithms",
        "implShamirsTrickJsf",
        "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_ecalg_impl_shamirs_trick_jsf,
    );

    }

#[derive(Clone, Copy)]
pub struct BcSecTFieldSpec {
    pub m: usize,
    pub ks: &'static [usize],
}

impl BcSecTFieldSpec {
    pub fn words(self) -> usize {
        (self.m + 63) >> 6
    }

    pub fn ext_words(self) -> usize {
        (self.m * 2 + 63) >> 6
    }
}

pub fn bc_sect_aioobe(index: usize) -> MethodCallFailed {
    RuntimeError::aioobe_index_only(index.min(i32::MAX as usize) as i32).into()
}

pub fn bc_read_long_array_fixed(
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

pub fn bc_write_long_array_fixed(
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

pub fn bc_xor_into_long_array_fixed(
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

pub fn bc_alloc_jlong_array(
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

pub fn bc_sect_half_trace(x: Vec<u64>, spec: BcSecTFieldSpec) -> Vec<u64> {
    let mut z = x.clone();
    for _ in (1..spec.m).step_by(2) {
        z = bc_poly_mod_square_n(z, 2, spec.m, spec.ks);
        z = bc_poly_add(z, &x);
    }
    z
}

pub fn bc_sect_trace(x: Vec<u64>, spec: BcSecTFieldSpec) -> i32 {
    let mut t = bc_poly_reduce(x, spec.m, spec.ks);
    let mut acc = t.clone();
    for _ in 1..spec.m {
        t = bc_poly_mod_square_n(t, 1, spec.m, spec.ks);
        acc = bc_poly_add(acc, &t);
    }
    (acc.first().copied().unwrap_or(0) & 1) as i32
}

pub fn bc_sect_add_native(
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

pub fn bc_sect_add_both_to_native(
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

pub fn bc_sect_add_ext_native(
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

pub fn bc_sect_add_one_native(
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

pub fn bc_sect_multiply_native(
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

pub fn bc_sect_multiply_add_to_ext_native(
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

pub fn bc_sect_reduce_native(
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

pub fn bc_sect_sqrt_native(
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

pub fn bc_sect_square_native(
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

pub fn bc_sect_square_add_to_ext_native(
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

pub fn bc_sect_square_n_native(
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

pub fn bc_sect_invert_native(
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

pub fn bc_sect_half_trace_native(
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

pub fn bc_sect_trace_native(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    spec: BcSecTFieldSpec,
) -> MethodCallResult {
    let x_arr = obj_arg(args, 0)?;
    let x = bc_read_long_array_fixed(ctx, x_arr, spec.words())?;
    Ok(Some(Value::Int(bc_sect_trace(x, spec))))
}

pub fn bc_sect_precomp_multiplicand_native(
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

pub fn bc_sect_multiply_precomp_native(
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

pub fn bc_sect_multiply_precomp_add_to_ext_native(
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

define_bc_sect_module!(bc_sect113, 113, [9]);

define_bc_sect_module!(bc_sect131, 131, [2, 3, 8]);

define_bc_sect_module!(bc_sect163, 163, [3, 6, 7]);

define_bc_sect_module!(bc_sect193, 193, [15]);

define_bc_sect_module!(bc_sect233, 233, [74]);

define_bc_sect_module!(bc_sect239, 239, [158]);

define_bc_sect_module!(bc_sect283, 283, [5, 7, 12]);

define_bc_sect_module!(bc_sect409, 409, [87]);

define_bc_sect_module!(bc_sect571, 571, [2, 5, 10]);
pub fn register_bc_sect571_precomp(r: &mut NativeMethodRegistry) {
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571Field",
        "precompMultiplicand",
        "([J)[J",
        bc_sect571::precomp_multiplicand,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571Field",
        "multiplyPrecomp",
        "([J[J[J)V",
        bc_sect571::multiply_precomp,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/custom/sec/SecT571Field",
        "multiplyPrecompAddToExt",
        "([J[J[J)V",
        bc_sect571::multiply_precomp_add_to_ext,
    );
}

pub fn register_bc_sect_field_kernels(r: &mut NativeMethodRegistry) {
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
    }

#[derive(Clone, Copy)]
pub enum BcSecTPointFormula {
    General,
    KoblitzZeroA,
    SecT163K1,
}

#[derive(Clone, Copy)]
pub struct BcSecTPointSpec {
    pub point_class: &'static str,
    pub field_class: &'static str,
    pub field: BcSecTFieldSpec,
    pub formula: BcSecTPointFormula,
}

pub struct BcSecTPointData {
    pub curve: ObjectRef,
    pub x: Vec<u64>,
    pub y: Vec<u64>,
    pub z: Vec<u64>,
    pub spec: BcSecTPointSpec,
}

pub fn bc_poly_is_zero(x: &[u64]) -> bool {
    x.iter().all(|&w| w == 0)
}

pub fn bc_poly_is_one(x: &[u64]) -> bool {
    x.first().copied().unwrap_or(0) == 1 && x.iter().skip(1).all(|&w| w == 0)
}

pub fn bc_sect_add2(a: &[u64], b: &[u64]) -> Vec<u64> {
    bc_poly_add(a.to_vec(), b)
}

pub fn bc_sect_add_many(first: &[u64], rest: &[&[u64]]) -> Vec<u64> {
    let mut out = first.to_vec();
    for part in rest {
        out = bc_poly_add(out, part);
    }
    out
}

pub fn bc_sect_mul(a: &[u64], b: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_reduce(bc_poly_mul_raw(a, b), spec.m, spec.ks)
}

pub fn bc_sect_square(a: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_reduce(bc_poly_square_raw(a), spec.m, spec.ks)
}

pub fn bc_sect_sqrt(a: &[u64], spec: BcSecTFieldSpec) -> Vec<u64> {
    bc_poly_mod_square_n(a.to_vec(), spec.m.saturating_sub(1) as i32, spec.m, spec.ks)
}

pub fn bc_sect_square_plus_product(
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

pub fn bc_sect_field_words(
    ctx: &dyn NativeContext,
    field_obj: ObjectRef,
    spec: BcSecTFieldSpec,
) -> Result<Vec<u64>, MethodCallFailed> {
    let arr = bc_fp_obj_field(ctx, field_obj, "x")?;
    bc_read_long_array_fixed(ctx, arr, spec.words())
}

pub fn bc_sect_curve_field_words(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
    name: &str,
    spec: BcSecTFieldSpec,
) -> Result<Vec<u64>, MethodCallFailed> {
    let field = bc_fp_obj_field(ctx, curve, name)?;
    bc_sect_field_words(ctx, field, spec)
}

pub fn bc_sect_point_infinity(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_fp_bad_state("SecTPoint: curve has no infinity point")),
    }
}

pub fn bc_sect_point_read(
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

pub fn bc_sect_alloc_field_element(
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

pub fn bc_sect_alloc_point(
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

pub fn bc_sect_point_double_data(
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

pub fn bc_sect_point_return_twice_with_spec(
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

pub fn bc_sect_point_spec_for_class(class_name: &str) -> Option<BcSecTPointSpec> {
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

pub fn bc_ec_point_return_times_pow2(
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

pub fn register_bc_sect_point_methods(r: &mut NativeMethodRegistry) {
    bc_register_null_rerun!(
        r,
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
}


pub fn bc_fp_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_fp_obj_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_fp_bad_state("ECFieldElement.Fp: malformed object field")),
    }
}

pub fn bc_fp_optional_obj_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Value {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Value::Object(Some(o)),
        _ => Value::Object(None),
    }
}

pub fn bc_fp_read_this(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(ObjectRef, Value, crate::bigint::BigInt), MethodCallFailed> {
    let q_obj = bc_fp_obj_field(ctx, this, "q")?;
    let r_val = bc_fp_optional_obj_field(ctx, this, "r");
    let x_obj = bc_fp_obj_field(ctx, this, "x")?;
    Ok((q_obj, r_val, bi_read_int(ctx, x_obj)))
}

pub fn bc_fp_arg_value(
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

pub fn bc_fp_mod_reduce(
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

pub fn bc_fp_alloc(
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

pub fn bc_fp_return(
    ctx: &mut dyn NativeContext,
    q_obj: ObjectRef,
    r_val: Value,
    x: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_fp_alloc(
        ctx, q_obj, r_val, &x,
    )?))))
}

pub fn bc_fp_return_bi(
    ctx: &mut dyn NativeContext,
    x: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &x)?))))
}

pub fn bc_fp_bigint_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    idx: usize,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    Ok(bi_read_int(ctx, obj_arg(args, idx)?))
}

pub fn bc_fp_q_r(
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
pub struct BcFpPoint {
    pub curve: ObjectRef,
    pub q_obj: ObjectRef,
    pub r_val: Value,
    pub q: crate::bigint::BigInt,
    pub a: crate::bigint::BigInt,
    pub x: crate::bigint::BigInt,
    pub y: crate::bigint::BigInt,
}

pub fn bc_fp_mod(
    x: crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    x.modulo(q)
}

pub fn bc_fp_add_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.add(b).modulo(q)
}

pub fn bc_fp_sub_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.sub(b).modulo(q)
}

pub fn bc_fp_mul_mod(
    a: &crate::bigint::BigInt,
    b: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> crate::bigint::BigInt {
    a.mul(b).modulo(q)
}

pub fn bc_fp_inverse_mod(
    a: &crate::bigint::BigInt,
    q: &crate::bigint::BigInt,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    a.mod_inverse(q).ok_or_else(|| {
        RuntimeError::ArithmeticException {
            // BouncyCastle's own text (`Mod.modOddInverse` /
            // `BigIntegers.modOddInverse`) ends in a full stop, as the JDK's
            // `BigInteger.modInverse` does (interpreter round i4 wave 98, L9).
            message: "BigInteger not invertible.".into(),
        }
        .into()
    })
}

pub fn bc_fp_field_x(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let x_obj = bc_fp_obj_field(ctx, obj, "x")?;
    Ok(bi_read_int(ctx, x_obj))
}

pub fn bc_fp_curve_coord(ctx: &dyn NativeContext, curve: ObjectRef) -> i32 {
    match ctx.get_field_by_name(curve, "coord") {
        Value::Int(coord) => coord,
        _ => 0,
    }
}

pub fn bc_fp_curve_a(
    ctx: &dyn NativeContext,
    curve: ObjectRef,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let a_obj = bc_fp_obj_field(ctx, curve, "a")?;
    bc_fp_field_x(ctx, a_obj)
}

pub fn bc_fp_point_infinity(ctx: &dyn NativeContext, curve: ObjectRef) -> MethodCallResult {
    match ctx.get_field_by_name(curve, "infinity") {
        Value::Object(Some(infinity)) => Ok(Some(Value::Object(Some(infinity)))),
        _ => Err(bc_fp_bad_state("ECPoint.Fp: curve has no infinity point")),
    }
}

pub fn bc_fp_point_read(
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

pub fn bc_fp_alloc_point(
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

pub fn bc_fp_point_from_xy(
    ctx: &mut dyn NativeContext,
    p: &BcFpPoint,
    x: crate::bigint::BigInt,
    y: crate::bigint::BigInt,
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(bc_fp_alloc_point(
        ctx, p.curve, p.q_obj, p.r_val, &p.q, x, y,
    )?))))
}

pub fn bc_fp_point_add_data(
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

pub fn bc_fp_point_double_data(
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

pub fn bc_fp_point_negate_data(
    p: &BcFpPoint,
) -> (crate::bigint::BigInt, crate::bigint::BigInt) {
    let y = if p.y.is_zero() {
        crate::bigint::BigInt::zero()
    } else {
        p.q.sub(&p.y).modulo(&p.q)
    };
    (p.x.clone(), y)
}

pub fn bc_fp_point_return_add(
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

pub fn bc_fp_point_return_twice(
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

pub fn bc_fp_point_return_negate(
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

pub fn bc_fp_point_return_twice_plus(
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

pub fn bc_fp_point_return_three_times(
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

pub fn bc_fp_point_return_times_pow2(
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

pub fn register_bc_fp_point(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/math/ec/ECPoint$Fp";
    bc_register_null_rerun!(
        r,
        CLS,
        "add",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_add,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "twice",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_twice,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "twicePlus",
        "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_twice_plus,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "threeTimes",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_three_times,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "timesPow2",
        "(I)Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_times_pow2,
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "negate",
        "()Lorg/bouncycastle/math/ec/ECPoint;",
        bc_fp_point_return_negate,
    );

    }

pub fn register_bc_fp_field_element(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/math/ec/ECFieldElement$Fp";

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    bc_register_null_rerun!(
        r,
        CLS,
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

    }

#[derive(Clone, Copy)]
pub enum BcDigestKind {
    Sha1,
    Sha256,
}

impl BcDigestKind {
    pub fn len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => crate::real_sha1(data),
            Self::Sha256 => crate::real_sha256(data),
        }
    }
}

pub fn bc_gost3411_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_gost3411_byte_field(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(digest, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_gost3411_bad_state("GOST3411Digest: malformed state")),
    }
}

pub fn bc_gost3411_read_32(
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

pub fn bc_gost3411_write_field(
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

pub fn bc_gost3411_read_c(
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

pub fn bc_gost3411_read_sbox(
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

pub fn bc_gost3411_a(block: &mut [u8; 32]) {
    let mut a = [0u8; 8];
    for j in 0..8 {
        a[j] = block[j] ^ block[j + 8];
    }
    block.copy_within(8..32, 0);
    block[24..32].copy_from_slice(&a);
}

pub fn bc_gost3411_p(input: &[u8; 32]) -> [u8; 32] {
    let mut key = [0u8; 32];
    for k in 0..8 {
        key[4 * k] = input[k];
        key[1 + 4 * k] = input[8 + k];
        key[2 + 4 * k] = input[16 + k];
        key[3 + 4 * k] = input[24 + k];
    }
    key
}

pub fn bc_gost3411_fw(block: &mut [u8; 32]) {
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

pub fn bc_gost28147_key(key: &[u8; 32]) -> [u32; 8] {
    let mut out = [0u32; 8];
    for i in 0..8 {
        let o = i * 4;
        out[i] = u32::from_le_bytes([key[o], key[o + 1], key[o + 2], key[o + 3]]);
    }
    out
}

pub fn bc_gost28147_main_step(sbox: &[u8; 128], n1: u32, key: u32) -> u32 {
    let cm = key.wrapping_add(n1);
    let mut om = 0u32;
    for i in 0..8 {
        let nibble = ((cm >> (i * 4)) & 0x0f) as usize;
        om |= (sbox[i * 16 + nibble] as u32) << (i * 4);
    }
    om.rotate_left(11)
}

pub fn bc_gost28147_encrypt_block(
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

pub fn bc_gost3411_process_block(
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
pub fn register_bc_gost3411_digest(r: &mut NativeMethodRegistry) {
        
    bc_register_rerun!(
        r,
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

    }

pub const BC_WHIRLPOOL_SBOX: [u8; 256] = [
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

pub struct BcWhirlpoolTables {
    pub c: [[u64; 256]; 8],
    pub rc: [u64; 11],
}

