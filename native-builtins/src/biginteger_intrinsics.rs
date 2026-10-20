// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H13 — `java.math.BigInteger` intrinsic-candidate native overrides.
//!
//! KC16's WildFly bootstrap exercises the SunJCE provider's static
//! initializer, which constructs a 2048-bit BigInteger and squares it
//! during DH/RSA parameter spec validation. In real-JDK mode our
//! interpreter runs the JDK bytecode for `BigInteger.implSquareToLen`,
//! `primitiveLeftShift`, `shiftLeftImplWorker`, `shiftRightImplWorker`,
//! `implMulAdd`, `mulAdd`, `implMultiplyToLen`, `implMontgomeryMultiply`
//! and `implMontgomerySquare`. These are tagged `@IntrinsicCandidate`
//! in real HotSpot — meaning the JIT replaces them with hand-rolled
//! C2 assembly. Without the intrinsic, the interpreted Java fallback
//! is so slow on a 2048-bit operand that KC16's main thread pegs at
//! 100% CPU for tens of seconds, missing the watchdog safepoint.
//!
//! This module registers spec-correct Rust implementations of each
//! intrinsic candidate, mirroring HotSpot's behaviour: the JDK's
//! Java fallback bytecode is bypassed, the native runs in microseconds
//! instead of seconds, and KC16 boot can advance.
//!
//! References: OpenJDK 25 `java.math.BigInteger` source, in particular
//! `implSquareToLen`, `mulAdd`, `implMulAdd`, `addOne`,
//! `primitiveLeftShift`, `primitiveRightShift`, `shiftLeftImplWorker`,
//! `shiftRightImplWorker`, and the squareToLen entry-point chain
//! routed through `square()`.
//!
//! Anchor: `T19_H13_BIGINTEGER_INTRINSICS`.
//!
//! Security posture:
//! * No unsafe code in this module.
//! * Every array index is bounds-checked before access; out-of-range
//!   inputs raise `ArrayIndexOutOfBoundsException` (matching the JDK's
//!   `Objects.checkFromToIndex` / explicit checks) rather than panic.
//! * Length / index parameters are validated against their array
//!   dimensions before any arithmetic; a malicious caller passing
//!   negative or oversized lengths gets a thrown exception, never
//!   undefined behaviour.
//! * Shift counts are clamped to the JDK-spec range (1..=31). The
//!   JDK's intrinsic also implicitly assumes `1..=31`; counts of `0`
//!   are filtered by the `primitiveLeftShift` / `primitiveRightShift`
//!   wrappers but we additionally clamp inside the workers as belt-and-
//!   suspenders so out-of-range counts cannot trigger a Rust shift-overflow
//!   panic in debug builds.
//! * No `unwrap()` on caller-provided values; only on the always-safe
//!   `MethodCallResult::Ok` constructor.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

/// `java.math.BigInteger.LONG_MASK = 0xFFFFFFFFL`.
const LONG_MASK: u64 = 0xFFFF_FFFF;

/// Helper — extract the int element at `idx` from an `int[]` array
/// reference, with explicit bounds checking. Returns
/// `ArrayIndexOutOfBoundsException` on overflow rather than letting
/// the underlying `get_array_element` silently zero-extend (which
/// would mask off-by-one bugs in the caller).
fn read_int(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    len: usize,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    if idx >= len {
        return Err(RuntimeError::aioobe_index_only(idx as i32).into());
    }
    match ctx.get_array_element(arr, idx) {
        Value::Int(v) => Ok(v),
        // `int[]` slot read as something else means the array was
        // mistyped by the caller; treat as AIOOBE-equivalent.
        _ => Err(RuntimeError::ArrayStoreException {
            message: "expected int[] element".to_string(),
        }
        .into()),
    }
}

/// Helper — write an int to `arr[idx]` with bounds checking.
fn write_int(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    idx: usize,
    len: usize,
    val: i32,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if idx >= len {
        return Err(RuntimeError::aioobe_index_only(idx as i32).into());
    }
    ctx.set_array_element(arr, idx, Value::Int(val));
    Ok(())
}

/// Pull an `int[]` ObjectRef out of a Value, returning `null` AIOOBE-equivalent
/// (Java spec: the JDK's intrinsic never receives null because the wrapper
/// always allocates first; defensive check anyway so a malformed bytecode
/// caller cannot crash us).
fn require_int_array(
    v: Option<&Value>,
    name: &str,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match v {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("{name} must not be null")),
        }
        .into()),
    }
}

fn require_int(
    v: Option<&Value>,
    name: &str,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    match v {
        Some(Value::Int(i)) => Ok(*i),
        _ => Err(RuntimeError::IllegalArgumentException {
            message: format!("{name} must be int"),
        }
        .into()),
    }
}

fn require_long(
    v: Option<&Value>,
    name: &str,
) -> Result<i64, cratonvm_types::error::MethodCallFailed> {
    match v {
        Some(Value::Long(i)) => Ok(*i),
        _ => Err(RuntimeError::IllegalArgumentException {
            message: format!("{name} must be long"),
        }
        .into()),
    }
}

// ---------------------------------------------------------------------------
// Public algorithm helpers (operate directly on Vec<i32> for testability).
// The native stubs below pull arrays out of the heap, then call into these.
// ---------------------------------------------------------------------------

/// Spec-compliant transcription of OpenJDK 25's `implSquareToLen`.
///
/// Computes `z = x*x` where `x` is a magnitude expressed as a big-endian
/// `int[]` of `len` 32-bit words. `z` is a destination buffer of length
/// `zlen >= 2*len`. Returns the populated `z` slice (always the input
/// `z`).
///
/// Algorithm (Colin Plumb's symmetric-multiply technique): build the
/// diagonal product first, shifted right by one bit; add the off-
/// diagonal sums; left-shift back by one and OR in the low bit.
pub fn impl_square_to_len(x: &[i32], len: usize, z: &mut [i32], zlen: usize) {
    debug_assert!(len >= 1);
    debug_assert!(len <= x.len());
    debug_assert!(zlen >= len * 2);
    debug_assert!(zlen <= z.len());

    // Pass 1 — store squares of digits, right-shifted by one.
    let mut last_product_low_word: u32 = 0;
    let mut i = 0usize;
    for j in 0..len {
        let piece: u64 = (x[j] as u32) as u64;
        let product: u64 = piece.wrapping_mul(piece);
        // z[i++] = (lastProductLowWord << 31) | (int)(product >>> 33);
        let high_part: u32 =
            ((last_product_low_word as u64) << 31) as u32 | ((product >> 33) as u32);
        z[i] = high_part as i32;
        i += 1;
        // z[i++] = (int)(product >>> 1);
        z[i] = ((product >> 1) as u32) as i32;
        i += 1;
        last_product_low_word = product as u32;
    }

    // Pass 2 — add off-diagonal sums.
    // `for (int i = len, offset = 1; i > 0; i--, offset += 2)`
    let mut row_i = len;
    let mut offset: usize = 1;
    while row_i > 0 {
        let t: i32 = x[row_i - 1];
        let carry = mul_add(z, x, offset, row_i - 1, t);
        add_one(z, offset - 1, row_i, carry);
        row_i -= 1;
        offset += 2;
    }

    // Pass 3 — shift back up and set low bit.
    primitive_left_shift_inplace(z, zlen, 1);
    z[zlen - 1] |= x[len - 1] & 1;
}

/// Spec-compliant transcription of OpenJDK 25's `mulAdd`.
///
/// JDK contract (note the index rewrite!):
/// ```text
/// static int mulAdd(int[] out, int[] in_, int offset, int len, int k) {
///     long kLong = k & LONG_MASK;
///     long carry = 0;
///     offset = out.length - offset - 1;       // <-- rewrite
///     for (int j = len - 1; j >= 0; j--) {
///         long product = (in_[j] & LONG_MASK) * kLong +
///                        (out[offset] & LONG_MASK) + carry;
///         out[offset--] = (int) product;
///         carry = product >>> 32;
///     }
///     return (int) carry;
/// }
/// ```
///
/// The first iteration writes `out[out.len() - offset - 1]`, the last
/// writes `out[out.len() - offset - len]`. Caller is responsible for
/// passing an `out` long enough to hold those writes.
pub fn mul_add(out: &mut [i32], in_: &[i32], offset: usize, len: usize, k: i32) -> i32 {
    let k_long: u64 = (k as u32) as u64;
    let mut carry: u64 = 0;
    // `offset = out.length - offset - 1` per JDK.
    if out.is_empty() {
        return 0;
    }
    if offset + 1 > out.len() {
        // No room — caller bug, but stay panic-free.
        return 0;
    }
    let mut off: i64 = out.len() as i64 - offset as i64 - 1;
    for j in (0..len).rev() {
        if off < 0 || off as usize >= out.len() {
            // Out-of-range — match JDK's AIOOBE rather than wrapping.
            // The intrinsic-spec callers never trigger this; defensive.
            break;
        }
        let off_u = off as usize;
        let product: u64 =
            ((in_[j] as u32) as u64).wrapping_mul(k_long) + ((out[off_u] as u32) as u64) + carry;
        out[off_u] = product as u32 as i32;
        carry = product >> 32;
        off -= 1;
    }
    carry as u32 as i32
}

/// Spec-compliant transcription of OpenJDK 25's `addOne`.
///
/// JDK contract:
/// ```text
/// static void addOne(int[] a, int offset, int mlen, int carry) {
///     offset = a.length - 1 - mlen - offset;     // <-- rewrite
///     long t = (a[offset] & LONG_MASK) + (carry & LONG_MASK);
///     a[offset] = (int) t;
///     if ((t >>> 32) == 0) return;
///     while (--mlen >= 0) {
///         if (--offset < 0) return;              // carry off the top
///         a[offset]++;
///         if (a[offset] != 0) return;
///     }
/// }
/// ```
pub fn add_one(a: &mut [i32], offset: usize, mlen: usize, carry: i32) {
    let _ = add_one_carry(a, offset, mlen, carry);
}

/// The same `addOne`, returning the JDK's `int` result (1 when the carry ran
/// off the top word, 0 otherwise).
///
/// `implSquareToLen` discards that value, which is why [`add_one`] above does
/// too; `montReduce` accumulates it (`c += addOne(...)`), so the reduction
/// needs the carry-returning spelling. One body, two spellings — the two must
/// never drift.
pub fn add_one_carry(a: &mut [i32], offset: usize, mlen: usize, carry: i32) -> i32 {
    if a.is_empty() {
        return 0;
    }
    // offset = a.length - 1 - mlen - offset
    let off_signed: i64 = a.len() as i64 - 1 - mlen as i64 - offset as i64;
    if off_signed < 0 || off_signed as usize >= a.len() {
        // Defensive: the JDK would raise AIOOBE here. No spec caller reaches
        // it; answer "no carry" rather than panicking.
        return 0;
    }
    let mut off = off_signed as usize;
    let t: u64 = ((a[off] as u32) as u64) + ((carry as u32) as u64);
    a[off] = t as u32 as i32;
    if (t >> 32) == 0 {
        return 0;
    }
    let mut m = mlen as i64;
    loop {
        m -= 1;
        if m < 0 {
            return 1;
        }
        if off == 0 {
            // JDK: `if (--offset < 0) return 1;` — the carry left the number.
            return 1;
        }
        off -= 1;
        // a[offset]++; if (a[offset] != 0) return 0;
        let new_val = (a[off] as u32).wrapping_add(1);
        a[off] = new_val as i32;
        if new_val != 0 {
            return 0;
        }
    }
}

/// Spec-compliant transcription of OpenJDK 25's `implMultiplyToLen`.
///
/// ```text
/// private static int[] implMultiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z) {
///     int xstart = xlen - 1;
///     int ystart = ylen - 1;
///     long carry = 0;
///     for (int j=ystart, k=ystart+1+xstart; j >= 0; j--, k--) {
///         long product = (y[j] & LONG_MASK) * (x[xstart] & LONG_MASK) + carry;
///         z[k] = (int)product;
///         carry = product >>> 32;
///     }
///     z[xstart] = (int)carry;
///     for (int i = xstart-1; i >= 0; i--) {
///         carry = 0;
///         for (int j=ystart, k=ystart+1+i; j >= 0; j--, k--) {
///             long product = (y[j] & LONG_MASK) * (x[i] & LONG_MASK)
///                          + (z[k] & LONG_MASK) + carry;
///             z[k] = (int)product;
///             carry = product >>> 32;
///         }
///         z[i] = (int)carry;
///     }
///     return z;
/// }
/// ```
///
/// Grade-school big-endian magnitude multiply. Only `z[0 .. xlen+ylen)` is
/// written; any words above that are left exactly as they were, which is what
/// the JDK bytecode does and what `montReduce` (which reads from `z.len()`,
/// not from `xlen + ylen`) depends on.
///
/// Returns `Err(index)` for the first access Java would have thrown
/// `ArrayIndexOutOfBoundsException` on, carrying that exception's index.
pub fn impl_multiply_to_len(
    x: &[i32],
    xlen: i32,
    y: &[i32],
    ylen: i32,
    z: &mut [i32],
) -> Result<(), i32> {
    // Every access below is checked in the ORDER the JDK bytecode performs it,
    // and the first one out of range reports the index Java would put in the
    // `ArrayIndexOutOfBoundsException`. That is why the lengths arrive as
    // `i32` rather than `usize`: `xlen = 0` makes `xstart` negative, and the
    // JDK's `z[xstart]` genuinely throws `AIOOBE(-1)` rather than doing
    // nothing. Simulating the access is how the exception rows stay exact
    // without a case analysis per malformed shape.
    #[inline]
    fn get(a: &[i32], i: i64) -> Result<u64, i32> {
        if i < 0 || i as usize >= a.len() {
            return Err(i as i32);
        }
        Ok((a[i as usize] as u32) as u64)
    }
    #[inline]
    fn put(a: &mut [i32], i: i64, v: u32) -> Result<(), i32> {
        if i < 0 || i as usize >= a.len() {
            return Err(i as i32);
        }
        a[i as usize] = v as i32;
        Ok(())
    }

    let xstart = xlen as i64 - 1;
    let ystart = ylen as i64 - 1;

    // First row: y * x[xstart], written into z[xstart+1 ..= xstart+ylen].
    let mut carry: u64 = 0;
    let mut j = ystart;
    // JDK: `k` starts at `ystart+1+xstart` and steps down with `j`.
    let mut k = ystart + 1 + xstart;
    while j >= 0 {
        let yj = get(y, j)?;
        let xs = get(x, xstart)?;
        let product: u64 = yj.wrapping_mul(xs) + carry;
        put(z, k, product as u32)?;
        carry = product >> 32;
        j -= 1;
        k -= 1;
    }
    put(z, xstart, carry as u32)?;

    // Remaining rows, accumulating into z.
    let mut i = xstart - 1;
    while i >= 0 {
        carry = 0;
        let mut j = ystart;
        let mut k = ystart + 1 + i;
        while j >= 0 {
            let yj = get(y, j)?;
            let xi = get(x, i)?;
            let zk = get(z, k)?;
            let product: u64 = yj.wrapping_mul(xi) + zk + carry;
            put(z, k, product as u32)?;
            carry = product >> 32;
            j -= 1;
            k -= 1;
        }
        put(z, i, carry as u32)?;
        i -= 1;
    }
    Ok(())
}

/// Spec-compliant transcription of OpenJDK 25's `subN` — subtract two equal-
/// length magnitudes held in the LOW `len` words of `a` and `b`, in place on
/// `a`, returning the borrow as the JDK's `int` (0 or -1).
///
/// ```text
/// private static int subN(int[] a, int[] b, int len) {
///     long sum = 0;
///     while (--len >= 0) {
///         sum = (a[len] & LONG_MASK) - (b[len] & LONG_MASK) + (sum >> 32);
///         a[len] = (int)sum;
///     }
///     return (int)(sum >> 32);
/// }
/// ```
///
/// Note the JDK's `sum >> 32` is an **arithmetic** shift on a `long`, so the
/// borrow propagates as -1; the transcription keeps `sum` as `i64` for exactly
/// that reason.
pub fn sub_n(a: &mut [i32], b: &[i32], len: usize) -> i32 {
    let mut sum: i64 = 0;
    let mut i = len;
    while i > 0 {
        i -= 1;
        if i >= a.len() || i >= b.len() {
            return 0; // defensive; no spec caller reaches it
        }
        sum = ((a[i] as u32) as i64) - ((b[i] as u32) as i64) + (sum >> 32);
        a[i] = sum as u32 as i32;
    }
    (sum >> 32) as i32
}

/// Spec-compliant transcription of OpenJDK 25's `intArrayCmpToLen` — unsigned
/// lexicographic compare of the first `len` words of two big-endian
/// magnitudes.
pub fn int_array_cmp_to_len(arg1: &[i32], arg2: &[i32], len: usize) -> i32 {
    for i in 0..len {
        if i >= arg1.len() || i >= arg2.len() {
            return 0; // defensive; no spec caller reaches it
        }
        let b1 = (arg1[i] as u32) as u64;
        let b2 = (arg2[i] as u32) as u64;
        if b1 < b2 {
            return -1;
        }
        if b1 > b2 {
            return 1;
        }
    }
    0
}

/// Spec-compliant transcription of OpenJDK 25's `montReduce` — Montgomery
/// reduction of the `2*mlen`-word product held in `n`, in place.
///
/// ```text
/// private static int[] montReduce(int[] n, int[] mod, int mlen, int inv) {
///     int c=0, len = mlen, offset=0;
///     do {
///         int nEnd = n[n.length-1-offset];
///         int carry = mulAdd(n, mod, offset, mlen, inv * nEnd);
///         c += addOne(n, offset, mlen, carry);
///         offset++;
///     } while (--len > 0);
///     while (c > 0) c += subN(n, mod, mlen);
///     while (intArrayCmpToLen(n, mod, mlen) >= 0) subN(n, mod, mlen);
///     return n;
/// }
/// ```
///
/// Every index inside is derived from `n.len()`, NOT from `2*mlen`, so the
/// caller must hand over the same array the JDK would have — see
/// [`impl_multiply_to_len`].
///
/// On return the reduced residue occupies `n[0 .. mlen)`; the low half has
/// been zeroed by the reduction. That is the layout `oddModPow` reads back
/// with `Arrays.copyOf(b, modLen)`.
pub fn mont_reduce(n: &mut [i32], mod_: &[i32], mlen: usize, inv: i32) {
    if n.is_empty() || mlen == 0 {
        return;
    }
    let mut c: i32 = 0;
    let mut len = mlen;
    let mut offset: usize = 0;

    loop {
        // `n[n.length - 1 - offset]`
        let idx = match (n.len()).checked_sub(1 + offset) {
            Some(i) => i,
            None => return, // defensive; no spec caller reaches it
        };
        let n_end = n[idx];
        let carry = mul_add(n, mod_, offset, mlen, inv.wrapping_mul(n_end));
        c = c.wrapping_add(add_one_carry(n, offset, mlen, carry));
        offset += 1;
        len -= 1;
        if len == 0 {
            break;
        }
    }

    while c > 0 {
        c = c.wrapping_add(sub_n(n, mod_, mlen));
    }
    while int_array_cmp_to_len(n, mod_, mlen) >= 0 {
        sub_n(n, mod_, mlen);
    }
}

/// Spec-compliant in-place left shift used by `implSquareToLen`'s
/// finishing step. Mirrors HotSpot's intrinsic for the `n in 1..=31`
/// case (the call site always passes `n=1`, but we accept the full
/// range so the intrinsic native-override is a complete drop-in).
///
/// Mutates `a[0..len]` such that `a[i] := a[i] << n | a[i+1] >>> (32-n)`,
/// with the final word receiving `a[len-1] << n`.
pub fn primitive_left_shift_inplace(a: &mut [i32], len: usize, n: u32) {
    if len == 0 || n == 0 {
        return;
    }
    debug_assert!(n < 32, "n out of range; JDK contract is 1..=31");
    let n = n & 31; // belt-and-suspenders: cannot panic in debug builds.
    let comp = 32 - n;
    // shiftLeftImplWorker(a, a, 0, n, len)
    let mut old_idx = 0;
    let mut new_idx = 0;
    while old_idx < len.saturating_sub(1) {
        let lo = (a[old_idx] as u32).wrapping_shl(n);
        let hi = (a[old_idx + 1] as u32).wrapping_shr(comp);
        a[new_idx] = (lo | hi) as i32;
        old_idx += 1;
        new_idx += 1;
    }
    // a[len-1] <<= n
    let last = (a[len - 1] as u32).wrapping_shl(n);
    a[len - 1] = last as i32;
}

/// `shiftLeftImplWorker` — heap-array form. Out-of-place left shift that
/// writes `numIter` words into `new_arr[new_idx..new_idx+numIter]` from
/// `old_arr[0..=numIter]`. Callers are responsible for sizing `new_arr`.
///
/// # It ran one iteration short until 2026-09-10, and the comment is why
///
/// The note that used to sit here said OpenJDK's intrinsic "does NOT write the
/// final word — `primitiveLeftShift` patches `a[len-1] <<= n` itself
/// afterwards. So we iterate `numIter - 1` times like the JDK." The first half
/// is true of `primitiveLeftShift`, which passes `numIter = len - 1` precisely
/// so that it can patch the last word itself. The second half does not follow,
/// and OpenJDK 25's own bytecode says so:
///
/// ```text
///   9: iload 6        // oldIdx
///  11: iload 4        // numIter
///  13: if_icmpge 42   // while (oldIdx < numIter)
/// ```
///
/// `numIter` iterations, not `numIter - 1`. The worker's contract and its one
/// caller's argument were conflated, so the subtraction was applied twice.
/// Measured against HotSpot 25.0.4+7 by calling the method reflectively
/// (`apps/probes/L2IntrinsicProbe.java`): **17 of the left worker's rows and 16
/// of the right worker's differed, every one of them the top word left at
/// zero**, while `implSquareToLen`, `implMulAdd` and `mulAdd` in the same file
/// were 0-diff over the same run.
pub fn shift_left_impl_worker(
    new_arr: &mut [i32],
    old_arr: &[i32],
    new_idx: usize,
    shift_count: u32,
    num_iter: usize,
) {
    if num_iter == 0 {
        return;
    }
    debug_assert!(shift_count < 32);
    let n = shift_count & 31;
    let comp = 32 - n;
    // The last pass reads `old_arr[num_iter]`, so a caller that sized
    // `old_arr` to exactly `num_iter` is out of contract. Return rather than
    // index: a panic inside a native aborts the VM, and the entry point turns
    // the same condition into the `ArrayIndexOutOfBoundsException` Java throws.
    if old_arr.len() <= num_iter || new_arr.len() < new_idx + num_iter {
        return;
    }
    let mut nidx = new_idx;
    let mut oidx = 0usize;
    while oidx < num_iter {
        let lo = (old_arr[oidx] as u32).wrapping_shl(n);
        let hi = (old_arr[oidx + 1] as u32).wrapping_shr(comp);
        new_arr[nidx] = (lo | hi) as i32;
        nidx += 1;
        oidx += 1;
    }
}

/// `shiftRightImplWorker` — heap-array form, mirrors OpenJDK 25's
/// intrinsic. Right-shifts the first `numIter` words of `old_arr` by
/// `shiftCount`, writing into `new_arr` starting at index
/// `numIter - 1` and walking backward; new_idx is the floor of the
/// destination range.
pub fn shift_right_impl_worker(
    new_arr: &mut [i32],
    old_arr: &[i32],
    new_idx: usize,
    shift_count: u32,
    num_iter: usize,
) {
    // OpenJDK with numIter == 0 sets nidx to -1 (newIdx == 0) or fails the
    // loop test immediately, so it writes nothing; a usize cannot take the
    // `num_iter - 1` step below, so the case is answered here.
    if num_iter == 0 {
        return;
    }
    debug_assert!(shift_count < 32);
    let n = shift_count & 31;
    let comp = 32 - n;
    // OpenJDK: nidx = (newIdx == 0) ? numIter - 1 : numIter
    let top = if new_idx == 0 { num_iter - 1 } else { num_iter };
    // The FIRST pass reads `old_arr[num_iter]`. Same reasoning as the left
    // worker: decline rather than panic.
    if old_arr.len() <= num_iter || new_arr.len() <= top {
        return;
    }
    let mut idx = num_iter;
    let mut nidx = top;
    while nidx >= new_idx {
        let hi = (old_arr[idx] as u32).wrapping_shr(n);
        let lo_shifted = (old_arr[idx - 1] as u32).wrapping_shl(comp);
        new_arr[nidx] = (hi | lo_shifted) as i32;
        if nidx == 0 {
            // Java decrements to -1 here and the `nidx >= newIdx` test ends
            // the loop. usize has no -1, so the exit moves in front.
            break;
        }
        nidx -= 1;
        idx -= 1;
    }
}

// ---------------------------------------------------------------------------
// Native entry-points — pulled from the heap, then dispatched to the
// algorithm helpers above.
// ---------------------------------------------------------------------------

/// Materialise an `int[]` heap object into a fresh `Vec<i32>` so the
/// algorithm helpers can run without callbacks for every read.
fn read_int_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
) -> Result<Vec<i32>, cratonvm_types::error::MethodCallFailed> {
    let n = ctx.array_length(arr);
    // Bulk path first: `read_int_array_into` is a single `memcpy` on the VM's
    // real implementation (see its doc comment -- added for exactly this kind
    // of per-word `get_array_element` hot loop). Falls back to the scalar
    // loop only if it comes back short, which the `require_int_array` callers
    // above should already have ruled out; kept so a genuine mismatch still
    // reports the same actionable error instead of a silent truncation.
    let mut out = vec![0i32; n];
    if ctx.read_int_array_into(arr, 0, &mut out) == n {
        return Ok(out);
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push(v),
            // Defensive: int-array element returned as something else means
            // a category mismatch elsewhere; AIOOBE here gives an actionable
            // exception rather than an opaque value-type panic.
            _ => {
                return Err(RuntimeError::ArrayStoreException {
                    message: format!("expected int[] element at index {i}, got non-int value"),
                }
                .into())
            }
        }
    }
    Ok(out)
}

/// The array access's AIOOBE ("Index i out of bounds for length n") for a
/// `usize` index and length (wave 100 L7).
fn bigint_aioobe(index: usize, length: usize) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::aioobe(
        i32::try_from(index).unwrap_or(i32::MAX),
        i32::try_from(length).unwrap_or(i32::MAX),
    )
    .into()
}

/// Write a `Vec<i32>` back into a heap `int[]` of identical length.
fn write_int_array(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    src: &[i32],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let n = ctx.array_length(arr);
    if src.len() > n {
        return Err(RuntimeError::aioobe_index_only(src.len() as i32).into());
    }
    // Bulk path first, same reasoning as `read_int_array`'s.
    if ctx.write_int_array_from(arr, 0, src) {
        return Ok(());
    }
    for (i, v) in src.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*v));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SBR wave 5, lane bigint (TLS-T1): the arithmetic kernels, exact by
// construction and fast where the JDK's own callers stand.
//
// The six entry points below (`implMultiplyToLen`, `implSquareToLen`,
// `implMulAdd`, `mulAdd`, `implMontgomeryMultiply`, `implMontgomerySquare`)
// used to validate their arguments with checks of their own and answer with
// messages of their own (`IllegalArgumentException: invalid args: ...`,
// `Array index out of range: 5`), and computed on 32-bit words through
// per-access bounds checks. `tools/probes/SbrW5Bigint.java` measured both
// against HotSpot 25.0.4+7 running the JDK's Java bodies (`-Xint`, and C2 with
// the intrinsics disabled): 5 of the 6 hashes differed on the edge rows, and
// RSA-2048 signing ran 8x HotSpot's time.
//
// Every entry point now runs through ONE model of the Java body:
//
// * [`Arena`] holds each distinct array argument once (a reference passed
//   twice is ONE array, so an aliased call sees its own writes the way the
//   bytecode does), and flushes back every array the body wrote, even when
//   the body throws part-way — the partial stores Java leaves behind.
// * The `j_*` functions are line-for-line transcriptions of the JDK 25 bodies
//   (`BigInteger.java`), `int` arithmetic wrapping, every array access checked
//   in bytecode order, throwing the exception HotSpot throws there: `Index i
//   out of bounds for length n`, the helpful-NPE texts naming the JDK's own
//   parameter (`"x"`, `"in"`, ...; `java.base` carries a LocalVariableTable,
//   measured), the `IllegalArgumentException` texts of `implMulAddCheck` and
//   `implSquareToLenChecks`, `multiplyToLenCheck`'s `Array index out of range`
//   and `Objects.requireNonNull`'s null message, `NegativeArraySizeException`.
// * The `fast_*` functions compute the SAME answer on 64-bit limbs, and only
//   run where a stated precondition proves the Java body cannot throw and its
//   result is a closed-form value (the product; `a*b*R^-1 mod N` with a zero
//   low half). Everything else — every malformed shape a reflective caller
//   can build — takes the transcription. The JDK's own callers
//   (`multiplyToLen`, `squareToLen`, `oddModPow` through `montgomeryMultiply`)
//   always satisfy the preconditions.
// ---------------------------------------------------------------------------

use cratonvm_types::error::MethodCallFailed;

/// A Java exception one of the transcribed bodies throws, in the words
/// HotSpot uses at that site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Jx {
    /// An array access: `Index i out of bounds for length n`.
    Index {
        index: i32,
        length: i32,
    },
    /// `new ArrayIndexOutOfBoundsException(int)`: `Array index out of range: i`.
    IndexOnly(i32),
    /// `NullPointerException`; `None` is a null `getMessage()`
    /// (`Objects.requireNonNull`), `Some` a helpful-NPE text.
    Npe(Option<String>),
    Iae(String),
    /// `Objects.checkFromToIndex`.
    Ioobe(String),
    Nase(i32),
    /// The Java body would never return (zero modulus in `montReduce`).
    NoTermination(&'static str),
}

impl Jx {
    fn into_failed(self) -> MethodCallFailed {
        match self {
            Jx::Index { index, length } => RuntimeError::aioobe(index, length).into(),
            Jx::IndexOnly(i) => RuntimeError::aioobe_index_only(i).into(),
            Jx::Npe(message) => RuntimeError::NullPointerException { message }.into(),
            Jx::Iae(message) => RuntimeError::IllegalArgumentException { message }.into(),
            Jx::Ioobe(m) => RuntimeError::IndexOutOfBoundsException { message: Some(m) }.into(),
            Jx::Nase(size) => RuntimeError::NegativeArraySizeException { size }.into(),
            Jx::NoTermination(what) => RuntimeError::ArithmeticException {
                message: format!(
                    "BigInteger.{what}: the JDK body does not terminate for this input"
                ),
            }
            .into(),
        }
    }
}

/// An array argument: `None` is Java `null`, `Some(i)` indexes [`Arena`].
type H = Option<usize>;

/// Every distinct array a call touches, copied out of the heap once.
pub(crate) struct Arena {
    arrs: Vec<Vec<i32>>,
    dirty: Vec<bool>,
    /// The heap array each slot was read from; `None` for an array the body
    /// allocated itself (`new int[zlen]`).
    refs: Vec<Option<ObjectRef>>,
}

impl Arena {
    fn new() -> Self {
        Arena {
            arrs: Vec::with_capacity(4),
            dirty: Vec::with_capacity(4),
            refs: Vec::with_capacity(4),
        }
    }

    /// Register one `int[]` argument. A reference already registered gets
    /// the same slot, so aliased arguments stay one array.
    fn arg(&mut self, ctx: &dyn NativeContext, v: Option<&Value>) -> Result<H, MethodCallFailed> {
        match v {
            None | Some(Value::Object(None)) => Ok(None),
            Some(Value::Object(Some(o))) => {
                for (i, r) in self.refs.iter().enumerate() {
                    if let Some(r) = r {
                        if std::ptr::eq(r.as_ptr(), o.as_ptr()) {
                            return Ok(Some(i));
                        }
                    }
                }
                let data = read_int_array(ctx, *o)?;
                self.arrs.push(data);
                self.dirty.push(false);
                self.refs.push(Some(*o));
                Ok(Some(self.arrs.len() - 1))
            }
            Some(_) => Err(RuntimeError::IllegalArgumentException {
                message: "expected an int[] argument".to_string(),
            }
            .into()),
        }
    }

    /// `new int[n]`.
    fn new_array(&mut self, n: i32) -> Result<usize, Jx> {
        if n < 0 {
            return Err(Jx::Nase(n));
        }
        self.arrs.push(vec![0i32; n as usize]);
        self.dirty.push(true);
        self.refs.push(None);
        Ok(self.arrs.len() - 1)
    }

    /// `a.length`.
    #[inline]
    fn length(&self, h: H, who: &str) -> Result<i32, Jx> {
        match h {
            Some(i) => Ok(self.arrs[i].len() as i32),
            None => Err(Jx::Npe(Some(format!(
                "Cannot read the array length because \"{who}\" is null"
            )))),
        }
    }

    /// `a[i]` (iaload).
    #[inline]
    fn ld(&self, h: H, i: i32, who: &str) -> Result<i32, Jx> {
        match h {
            Some(s) => {
                let a = &self.arrs[s];
                if i < 0 || i as usize >= a.len() {
                    return Err(Jx::Index {
                        index: i,
                        length: a.len() as i32,
                    });
                }
                Ok(a[i as usize])
            }
            None => Err(Jx::Npe(Some(format!(
                "Cannot load from int array because \"{who}\" is null"
            )))),
        }
    }

    /// `a[i] = v` (iastore).
    #[inline]
    fn st(&mut self, h: H, i: i32, v: i32, who: &str) -> Result<(), Jx> {
        match h {
            Some(s) => {
                let a = &mut self.arrs[s];
                if i < 0 || i as usize >= a.len() {
                    return Err(Jx::Index {
                        index: i,
                        length: a.len() as i32,
                    });
                }
                a[i as usize] = v;
                self.dirty[s] = true;
                Ok(())
            }
            None => Err(Jx::Npe(Some(format!(
                "Cannot store to int array because \"{who}\" is null"
            )))),
        }
    }

    fn same(a: H, b: H) -> bool {
        matches!((a, b), (Some(x), Some(y)) if x == y)
    }

    /// Flush every heap array the body wrote (on success AND on a throw: the
    /// stores before the throw happened), then turn the body's result into a
    /// `Value`, allocating the heap array for a result the body created.
    ///
    /// GC-SAFETY: every heap array was copied out when it was registered, the
    /// flush writes through references that were live on entry, and the one
    /// allocation comes after the last write through an old reference.
    fn finish(
        self,
        ctx: &mut dyn NativeContext,
        result: Result<Option<H>, Jx>,
    ) -> MethodCallResult {
        let Arena { arrs, dirty, refs } = self;
        for i in 0..arrs.len() {
            if dirty[i] {
                if let Some(r) = refs[i] {
                    write_int_array(ctx, r, &arrs[i])?;
                }
            }
        }
        match result {
            Err(e) => Err(e.into_failed()),
            Ok(None) => Ok(None),
            Ok(Some(None)) => Ok(Some(Value::Object(None))),
            Ok(Some(Some(i))) => match refs[i] {
                Some(r) => Ok(Some(Value::Object(Some(r)))),
                None => {
                    let out = ctx.new_array(ArrayElementType::Int, arrs[i].len());
                    write_int_array(ctx, out, &arrs[i])?;
                    Ok(Some(Value::Object(Some(out))))
                }
            },
        }
    }
}

#[inline]
fn uw(v: i32) -> u64 {
    v as u32 as u64
}

#[cfg(test)]
thread_local! {
    /// Test-only: run every entry point through the transcription alone, so
    /// the tests can hold each fast path to the body it replaces.
    static FORCE_TRANSCRIPTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline(always)]
fn fast_paths_on() -> bool {
    #[cfg(test)]
    {
        !FORCE_TRANSCRIPTION.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

// --- The transcriptions ---------------------------------------------------

/// `private static int[] implMultiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z)`.
fn j_impl_multiply_to_len(ar: &mut Arena, x: H, xlen: i32, y: H, ylen: i32, z: H) -> Result<H, Jx> {
    if fast_multiply_to_len(ar, x, xlen, y, ylen, z) {
        return Ok(z);
    }
    let xstart = xlen.wrapping_sub(1);
    let ystart = ylen.wrapping_sub(1);
    let mut carry: u64 = 0;
    let mut j = ystart;
    let mut k = ystart.wrapping_add(1).wrapping_add(xstart);
    while j >= 0 {
        let product = uw(ar.ld(y, j, "y")?) * uw(ar.ld(x, xstart, "x")?) + carry;
        ar.st(z, k, product as i32, "z")?;
        carry = product >> 32;
        j -= 1;
        k = k.wrapping_sub(1);
    }
    ar.st(z, xstart, carry as i32, "z")?;
    let mut i = xstart.wrapping_sub(1);
    while i >= 0 {
        carry = 0;
        let mut j = ystart;
        let mut k = ystart.wrapping_add(1).wrapping_add(i);
        while j >= 0 {
            let product =
                uw(ar.ld(y, j, "y")?) * uw(ar.ld(x, i, "x")?) + uw(ar.ld(z, k, "z")?) + carry;
            ar.st(z, k, product as i32, "z")?;
            carry = product >> 32;
            j -= 1;
            k = k.wrapping_sub(1);
        }
        ar.st(z, i, carry as i32, "z")?;
        i -= 1;
    }
    Ok(z)
}

/// `private static void multiplyToLenCheck(int[] array, int length)`.
fn j_multiply_to_len_check(ar: &Arena, array: H, length: i32) -> Result<(), Jx> {
    if length <= 0 {
        return Ok(());
    }
    if array.is_none() {
        return Err(Jx::Npe(None)); // Objects.requireNonNull(array)
    }
    if length > ar.length(array, "array")? {
        return Err(Jx::IndexOnly(length - 1));
    }
    Ok(())
}

/// `private static int[] multiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z)`.
fn j_multiply_to_len(ar: &mut Arena, x: H, xlen: i32, y: H, ylen: i32, z: H) -> Result<H, Jx> {
    j_multiply_to_len_check(ar, x, xlen)?;
    j_multiply_to_len_check(ar, y, ylen)?;
    let need = xlen.wrapping_add(ylen);
    let z = if z.is_none() || ar.length(z, "z")? < need {
        Some(ar.new_array(need)?)
    } else {
        z
    };
    j_impl_multiply_to_len(ar, x, xlen, y, ylen, z)
}

/// `private static void implMulAddCheck(int[] out, int[] in, int offset, int len, int k)`.
fn j_mul_add_check(ar: &Arena, out: H, in_: H, offset: i32, len: i32) -> Result<(), Jx> {
    let in_len = ar.length(in_, "in")?;
    if len > in_len {
        return Err(Jx::Iae(format!(
            "input length is out of bound: {len} > {in_len}"
        )));
    }
    if offset < 0 {
        return Err(Jx::Iae(format!("input offset is invalid: {offset}")));
    }
    let out_len = ar.length(out, "out")?;
    if offset > out_len - 1 {
        return Err(Jx::Iae(format!(
            "input offset is out of bound: {offset} > {}",
            out_len - 1
        )));
    }
    if len > out_len - offset {
        return Err(Jx::Iae(format!(
            "input len is out of bound: {len} > {}",
            out_len - offset
        )));
    }
    Ok(())
}

/// `private static int implMulAdd(int[] out, int[] in, int offset, int len, int k)`.
fn j_impl_mul_add(
    ar: &mut Arena,
    out: H,
    in_: H,
    offset: i32,
    len: i32,
    k: i32,
) -> Result<i32, Jx> {
    let k_long = uw(k);
    let mut carry: u64 = 0;
    let mut off = ar.length(out, "out")?.wrapping_sub(offset).wrapping_sub(1);
    if len <= 0 {
        return Ok(0); // no iteration: nothing loaded, carry 0
    }
    // Fast form of the same loop when every index it touches is in range.
    if let (Some(o), Some(i), true) = (out, in_, fast_paths_on()) {
        let lowest = off as i64 - (len as i64 - 1);
        if len as usize <= ar.arrs[i].len() && (off as i64) < ar.arrs[o].len() as i64 && lowest >= 0
        {
            if o == i {
                // `out` and `in` are one array: keep the bytecode's order.
                for j in (0..len as usize).rev() {
                    let p = uw(ar.arrs[i][j]) * k_long + uw(ar.arrs[o][off as usize]) + carry;
                    ar.arrs[o][off as usize] = p as i32;
                    carry = p >> 32;
                    off -= 1;
                }
            } else {
                let (inv, outv) = two_mut(&mut ar.arrs, i, o);
                for j in (0..len as usize).rev() {
                    let p = uw(inv[j]) * k_long + uw(outv[off as usize]) + carry;
                    outv[off as usize] = p as i32;
                    carry = p >> 32;
                    off -= 1;
                }
            }
            ar.dirty[o] = true;
            return Ok(carry as i32);
        }
    }
    let mut j = len.wrapping_sub(1);
    while j >= 0 {
        let product = uw(ar.ld(in_, j, "in")?) * k_long + uw(ar.ld(out, off, "out")?) + carry;
        ar.st(out, off, product as i32, "out")?;
        off = off.wrapping_sub(1);
        carry = product >> 32;
        j -= 1;
    }
    Ok(carry as i32)
}

/// Borrow two distinct slots at once (`a` shared, `b` mutable).
fn two_mut(v: &mut [Vec<i32>], a: usize, b: usize) -> (&[i32], &mut [i32]) {
    debug_assert_ne!(a, b);
    if a < b {
        let (lo, hi) = v.split_at_mut(b);
        (&lo[a], &mut hi[0])
    } else {
        let (lo, hi) = v.split_at_mut(a);
        (&hi[0], &mut lo[b])
    }
}

/// `static int mulAdd(int[] out, int[] in, int offset, int len, int k)`.
fn j_mul_add(ar: &mut Arena, out: H, in_: H, offset: i32, len: i32, k: i32) -> Result<i32, Jx> {
    j_mul_add_check(ar, out, in_, offset, len)?;
    j_impl_mul_add(ar, out, in_, offset, len, k)
}

/// `static int addOne(int[] a, int offset, int mlen, int carry)`.
fn j_add_one(ar: &mut Arena, a: H, offset: i32, mlen: i32, carry: i32) -> Result<i32, Jx> {
    let mut off = ar
        .length(a, "a")?
        .wrapping_sub(1)
        .wrapping_sub(mlen)
        .wrapping_sub(offset);
    let t = uw(ar.ld(a, off, "a")?) + uw(carry);
    ar.st(a, off, t as i32, "a")?;
    if (t >> 32) == 0 {
        return Ok(0);
    }
    let mut m = mlen;
    loop {
        m = m.wrapping_sub(1);
        if m < 0 {
            return Ok(1);
        }
        off = off.wrapping_sub(1);
        if off < 0 {
            return Ok(1);
        }
        let v = ar.ld(a, off, "a")?.wrapping_add(1);
        ar.st(a, off, v, "a")?;
        if v != 0 {
            return Ok(0);
        }
    }
}

/// `private static int[] montReduce(int[] n, int[] mod, int mlen, int inv)`.
///
/// The `do` loop is transcribed. The two loops after it,
///
/// ```text
/// while (c > 0) c += subN(n, mod, mlen);
/// while (intArrayCmpToLen(n, mod, mlen) >= 0) subN(n, mod, mlen);
/// ```
///
/// are computed in closed form: they subtract `M = mod[0..mlen)` from
/// `V = c * 2^(32*mlen) + n[0..mlen)` until `V < M`, which leaves
/// `n[0..mlen) = V mod M` (the first loop stops the first time the borrow
/// cancels `c`, the second finishes the job). Neither can throw once the `do`
/// loop is through: its `mulAdd` checks already proved `mod.length >= mlen`
/// and `n.length >= mlen`. The closed form is what keeps a reflective caller
/// with a tiny modulus from spinning `2^32` iterations inside a native (the
/// Java body really does take that long, and HotSpot's own intrinsic does not
/// run it); the answer is the one the loops reach. For `M == 0` (or
/// `mlen <= 0`) the loops never end in Java; that is the one input answered
/// with an exception instead (`Jx::NoTermination`).
fn j_mont_reduce(ar: &mut Arena, n: H, mod_: H, mlen: i32, inv: i32) -> Result<H, Jx> {
    let mut c: i32 = 0;
    let mut len = mlen;
    let mut offset: i32 = 0;
    loop {
        let nl = ar.length(n, "n")?;
        let n_end = ar.ld(n, nl.wrapping_sub(1).wrapping_sub(offset), "n")?;
        let carry = j_mul_add(ar, n, mod_, offset, mlen, inv.wrapping_mul(n_end))?;
        c = c.wrapping_add(j_add_one(ar, n, offset, mlen, carry)?);
        offset = offset.wrapping_add(1);
        len = len.wrapping_sub(1);
        if len <= 0 {
            break;
        }
    }
    // The `do` loop dereferenced both (`n.length`, `mulAdd`'s `in.length`).
    let (Some(ns), Some(ms)) = (n, mod_) else {
        return Err(Jx::Npe(None));
    };
    if mlen <= 0 {
        return Err(Jx::NoTermination("montReduce"));
    }
    let ml = mlen as usize;
    let m = be_words_to_biguint(&ar.arrs[ms][..ml]);
    if m == num_bigint::BigUint::default() {
        return Err(Jx::NoTermination("montReduce"));
    }
    let top = be_words_to_biguint(&ar.arrs[ns][..ml]);
    let v = (num_bigint::BigUint::from(c as u32) << (32 * ml)) + top;
    let r = v % m;
    let digits = r.to_u32_digits(); // little-endian
    let dst = &mut ar.arrs[ns][..ml];
    for (i, w) in dst.iter_mut().rev().enumerate() {
        *w = digits.get(i).copied().unwrap_or(0) as i32;
    }
    ar.dirty[ns] = true;
    Ok(n)
}

fn be_words_to_biguint(w: &[i32]) -> num_bigint::BigUint {
    let le: Vec<u32> = w.iter().rev().map(|&v| v as u32).collect();
    num_bigint::BigUint::new(le)
}

/// `private static void shiftLeftImplWorker(int[] newArr, int[] oldArr, int newIdx, int shiftCount, int numIter)`
/// — the Java body, as `primitiveLeftShift` runs it inside `implSquareToLen`.
fn j_shift_left_impl_worker(
    ar: &mut Arena,
    new_arr: H,
    old_arr: H,
    new_idx: i32,
    shift_count: i32,
    num_iter: i32,
) -> Result<(), Jx> {
    let right = 32i32.wrapping_sub(shift_count);
    let mut old = 0i32;
    let mut ni = new_idx;
    while old < num_iter {
        let at = ni;
        ni = ni.wrapping_add(1);
        let a = ar.ld(old_arr, old, "oldArr")?;
        old += 1;
        let b = ar.ld(old_arr, old, "oldArr")?;
        let v = a.wrapping_shl(shift_count as u32) | ((b as u32).wrapping_shr(right as u32) as i32);
        ar.st(new_arr, at, v, "newArr")?;
    }
    Ok(())
}

/// `static void primitiveLeftShift(int[] a, int len, int n)`.
fn j_primitive_left_shift(ar: &mut Arena, a: H, len: i32, n: i32) -> Result<(), Jx> {
    if len == 0 || n == 0 {
        return Ok(());
    }
    let al = ar.length(a, "a")?;
    if len < 0 || len > al {
        return Err(Jx::Ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                0,
                i64::from(len),
                i64::from(al),
            ),
        ));
    }
    j_shift_left_impl_worker(ar, a, a, 0, n, len - 1)?;
    let v = ar.ld(a, len - 1, "a")?;
    ar.st(a, len - 1, v.wrapping_shl(n as u32), "a")
}

/// `private static final int[] implSquareToLen(int[] x, int len, int[] z, int zlen)`.
fn j_impl_square_to_len(ar: &mut Arena, x: H, len: i32, z: H, zlen: i32) -> Result<H, Jx> {
    if fast_square_to_len(ar, x, len, z, zlen) {
        return Ok(z);
    }
    let mut last: i32 = 0;
    let mut i: i32 = 0;
    let mut j: i32 = 0;
    while j < len {
        let piece = uw(ar.ld(x, j, "x")?);
        let product = piece.wrapping_mul(piece);
        let at = i;
        i = i.wrapping_add(1);
        ar.st(z, at, last.wrapping_shl(31) | ((product >> 33) as i32), "z")?;
        let at = i;
        i = i.wrapping_add(1);
        ar.st(z, at, (product >> 1) as i32, "z")?;
        last = product as i32;
        j += 1;
    }
    let mut row = len;
    let mut offset: i32 = 1;
    while row > 0 {
        let t = ar.ld(x, row - 1, "x")?;
        let t = j_mul_add(ar, z, x, offset, row - 1, t)?;
        j_add_one(ar, z, offset.wrapping_sub(1), row, t)?;
        row -= 1;
        offset = offset.wrapping_add(2);
    }
    j_primitive_left_shift(ar, z, zlen, 1)?;
    // z[zlen-1] |= x[len-1] & 1;   (z is loaded first)
    let zi = zlen.wrapping_sub(1);
    let zv = ar.ld(z, zi, "z")?;
    let xv = ar.ld(x, len.wrapping_sub(1), "x")?;
    ar.st(z, zi, zv | (xv & 1), "z")?;
    Ok(z)
}

/// `private static final int[] squareToLen(int[] x, int len, int[] z)` with
/// `implSquareToLenChecks`.
fn j_square_to_len(ar: &mut Arena, x: H, len: i32, z: H) -> Result<H, Jx> {
    let zlen = len.wrapping_shl(1);
    let z = if z.is_none() || ar.length(z, "z")? < zlen {
        Some(ar.new_array(zlen)?)
    } else {
        z
    };
    if len < 1 {
        return Err(Jx::Iae(format!("invalid input length: {len}")));
    }
    let xl = ar.length(x, "x")?;
    if len > xl {
        return Err(Jx::Iae(format!("input length out of bound: {len} > {xl}")));
    }
    let zl = ar.length(z, "z")?;
    if len.wrapping_mul(2) > zl {
        return Err(Jx::Iae(format!(
            "input length out of bound: {} > {zl}",
            len.wrapping_mul(2)
        )));
    }
    if zlen < 1 {
        return Err(Jx::Iae(format!("invalid input length: {zlen}")));
    }
    if zlen > zl {
        // The JDK prints `len` here, not `zlen`.
        return Err(Jx::Iae(format!("input length out of bound: {len} > {zl}")));
    }
    j_impl_square_to_len(ar, x, len, z, zlen)
}

/// `private static int[] implMontgomeryMultiply(int[] a, int[] b, int[] n, int len, long inv, int[] product)`.
fn j_impl_montgomery_multiply(
    ar: &mut Arena,
    a: H,
    b: H,
    n: H,
    len: i32,
    inv: i64,
    product: H,
) -> Result<H, Jx> {
    if let Some(r) = fast_montgomery(ar, a, b, n, len, inv, product) {
        return Ok(r);
    }
    let product = j_multiply_to_len(ar, a, len, b, len, product)?;
    j_mont_reduce(ar, product, n, len, inv as i32)
}

/// `private static int[] implMontgomerySquare(int[] a, int[] n, int len, long inv, int[] product)`.
fn j_impl_montgomery_square(
    ar: &mut Arena,
    a: H,
    n: H,
    len: i32,
    inv: i64,
    product: H,
) -> Result<H, Jx> {
    if let Some(r) = fast_montgomery(ar, a, a, n, len, inv, product) {
        return Ok(r);
    }
    let product = j_square_to_len(ar, a, len, product)?;
    j_mont_reduce(ar, product, n, len, inv as i32)
}

// --- 64-bit limb kernels ----------------------------------------------------

/// Big-endian 32-bit words -> little-endian 64-bit limbs (`out` pre-sized to
/// `ceil(w.len() / 2)`, zeroed).
#[inline]
fn be_to_limbs(w: &[i32], out: &mut [u64]) {
    let n = w.len();
    for l in out.iter_mut() {
        *l = 0;
    }
    for i in 0..n {
        out[i >> 1] |= uw(w[n - 1 - i]) << (32 * (i & 1));
    }
}

/// Little-endian 64-bit limbs -> the low `out.len()` big-endian 32-bit words.
#[inline]
fn limbs_to_be(l: &[u64], out: &mut [i32]) {
    let n = out.len();
    for i in 0..n {
        out[n - 1 - i] = (l[i >> 1] >> (32 * (i & 1))) as u32 as i32;
    }
}

/// `out = a * b` (`out.len() == a.len() + b.len()`).
#[inline]
fn mul_limbs(a: &[u64], b: &[u64], out: &mut [u64]) {
    for o in out.iter_mut() {
        *o = 0;
    }
    for (i, &ai) in a.iter().enumerate() {
        let ai = ai as u128;
        let mut c: u64 = 0;
        let row = &mut out[i..i + b.len() + 1];
        for (j, &bj) in b.iter().enumerate() {
            let v = ai * bj as u128 + row[j] as u128 + c as u128;
            row[j] = v as u64;
            c = (v >> 64) as u64;
        }
        row[b.len()] = c;
    }
}

/// `out = a^2` (`out.len() == 2 * a.len()`): off-diagonal products once,
/// doubled, plus the diagonal.
#[inline]
fn sqr_limbs(a: &[u64], out: &mut [u64]) {
    let s = a.len();
    for o in out.iter_mut() {
        *o = 0;
    }
    for i in 0..s {
        let ai = a[i] as u128;
        let mut c: u64 = 0;
        for j in i + 1..s {
            let v = ai * a[j] as u128 + out[i + j] as u128 + c as u128;
            out[i + j] = v as u64;
            c = (v >> 64) as u64;
        }
        out[i + s] = c;
    }
    let mut top = 0u64;
    for o in out.iter_mut() {
        let nt = *o >> 63;
        *o = (*o << 1) | top;
        top = nt;
    }
    let mut c: u64 = 0;
    for i in 0..s {
        let d = a[i] as u128 * a[i] as u128;
        let v = out[2 * i] as u128 + (d as u64) as u128 + c as u128;
        out[2 * i] = v as u64;
        let v2 = out[2 * i + 1] as u128 + (d >> 64) + (v >> 64);
        out[2 * i + 1] = v2 as u64;
        c = (v2 >> 64) as u64;
    }
}

/// `-m0^-1 mod 2^64` for an odd `m0`.
#[inline]
fn neg_inv64(m0: u64) -> u64 {
    let mut x = m0; // correct to 3 bits for odd m0
    for _ in 0..5 {
        x = x.wrapping_mul(2u64.wrapping_sub(m0.wrapping_mul(x)));
    }
    x.wrapping_neg()
}

/// Montgomery-reduce the `2s`-limb `t` (`t < m * R`), `R = 2^(64 s)`, into
/// `out = t * R^-1 mod m`, given `t * R^-1 < 2m` (true when `t = a*b`,
/// `a, b < m`). `t` is consumed as scratch.
#[inline]
fn redc_limbs(t: &mut [u64], m: &[u64], m_inv: u64, out: &mut [u64]) {
    let s = m.len();
    let mut hi_carry: u64 = 0; // carry out of the top of `t`
    for i in 0..s {
        let q = t[i].wrapping_mul(m_inv) as u128;
        let mut c: u64 = 0;
        for j in 0..s {
            let v = q * m[j] as u128 + t[i + j] as u128 + c as u128;
            t[i + j] = v as u64;
            c = (v >> 64) as u64;
        }
        // Propagate `c` into t[i+s ..].
        let mut k = i + s;
        let mut cc = c;
        while cc != 0 {
            if k == 2 * s {
                hi_carry += cc;
                break;
            }
            let (v, o) = t[k].overflowing_add(cc);
            t[k] = v;
            cc = o as u64;
            k += 1;
        }
    }
    // U = t[s..2s] + hi_carry * R, U < 2m.
    let u = &t[s..2 * s];
    let ge = hi_carry != 0 || !limbs_lt(u, m);
    if ge {
        let mut borrow = 0u64;
        for j in 0..s {
            let (d1, b1) = u[j].overflowing_sub(m[j]);
            let (d2, b2) = d1.overflowing_sub(borrow);
            out[j] = d2;
            borrow = (b1 | b2) as u64;
        }
    } else {
        out[..s].copy_from_slice(u);
    }
}

/// `a < b` for equal-length little-endian limb vectors.
#[inline]
fn limbs_lt(a: &[u64], b: &[u64]) -> bool {
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i] < b[i];
        }
    }
    false
}

/// `a[0..len) < n[0..len)` as big-endian unsigned magnitudes.
#[inline]
fn words_lt(a: &[i32], n: &[i32]) -> bool {
    for i in 0..a.len() {
        let (x, y) = (a[i] as u32, n[i] as u32);
        if x != y {
            return x < y;
        }
    }
    false
}

/// `implMultiplyToLen` where the Java body provably cannot throw and writes
/// exactly the product: `x`, `y`, `z` non-null, `z` distinct from both
/// operands (an aliased `z` is read back mid-product by the Java loop),
/// `1 <= xlen <= x.length`, `1 <= ylen <= y.length`,
/// `xlen + ylen <= z.length`. Writes `z[0 .. xlen+ylen)`; words above are
/// untouched, as in Java.
fn fast_multiply_to_len(ar: &mut Arena, x: H, xlen: i32, y: H, ylen: i32, z: H) -> bool {
    let (Some(xs), Some(ys), Some(zs)) = (x, y, z) else {
        return false;
    };
    if !fast_paths_on() || zs == xs || zs == ys || xlen < 1 || ylen < 1 {
        return false;
    }
    let (xl, yl, zl) = (xlen as usize, ylen as usize, ar.arrs[zs].len());
    if xl > ar.arrs[xs].len() || yl > ar.arrs[ys].len() || xl + yl > zl {
        return false;
    }
    let (lx, ly) = ((xl + 1) / 2, (yl + 1) / 2);
    let mut buf = vec![0u64; 2 * (lx + ly)];
    let (a, rest) = buf.split_at_mut(lx);
    let (b, prod) = rest.split_at_mut(ly);
    be_to_limbs(&ar.arrs[xs][..xl], a);
    be_to_limbs(&ar.arrs[ys][..yl], b);
    let prod = &mut prod[..lx + ly];
    if xs == ys && xl == yl {
        sqr_limbs(a, prod);
    } else {
        mul_limbs(a, b, prod);
    }
    limbs_to_be(prod, &mut ar.arrs[zs][..xl + yl]);
    ar.dirty[zs] = true;
    true
}

/// `implSquareToLen` where the Java body provably writes exactly `x^2`:
/// `x`, `z` non-null and distinct, `1 <= len <= x.length`, and
/// `z.length == zlen == 2*len` (every index inside `mulAdd`/`addOne` is taken
/// from `z.length`, so any other length changes the answer — those take the
/// transcription).
fn fast_square_to_len(ar: &mut Arena, x: H, len: i32, z: H, zlen: i32) -> bool {
    let (Some(xs), Some(zs)) = (x, z) else {
        return false;
    };
    if !fast_paths_on() || xs == zs || len < 1 || len as usize > ar.arrs[xs].len() {
        return false;
    }
    let l = len as usize;
    if ar.arrs[zs].len() != 2 * l || zlen as i64 != 2 * len as i64 {
        return false;
    }
    let s = (l + 1) / 2;
    let mut buf = vec![0u64; 3 * s];
    let (a, prod) = buf.split_at_mut(s);
    be_to_limbs(&ar.arrs[xs][..l], a);
    sqr_limbs(a, prod);
    limbs_to_be(prod, &mut ar.arrs[zs][..2 * l]);
    ar.dirty[zs] = true;
    true
}

/// `implMontgomeryMultiply` / `implMontgomerySquare` (`b == a`) on 64-bit
/// limbs, where the Java body's result is a closed form:
///
/// * `a`, `b`, `n` non-null; `len` even and `>= 2`; `len <= a.length`,
///   `b.length`, `n.length`;
/// * the product buffer is null or shorter than `2*len` (Java allocates
///   `new int[2*len]`) or exactly `2*len` long and distinct from `a`, `b`,
///   `n`;
/// * `(int)inv * n[len-1] == -1 (mod 2^32)` — the reduction constant is right,
///   so every `montReduce` step zeroes its word;
/// * `a, b < N` where `N = n[0..len)`.
///
/// Then no access in the Java body is out of range, `multiplyToLen` /
/// `squareToLen` leave `T = a*b` in the `2*len` words, and `montReduce`
/// leaves `T * R^-1 mod N` (`R = 2^(32*len)`) in the high half and zeros in
/// the low half: the word-by-word `q` it builds is the unique
/// `Q < R` with `T + Q*N == 0 (mod R)`, whatever the word size, so the value
/// `(T + Q*N) / R < 2N` is the same one a 64-bit-limb reduction builds, and
/// its two final loops reduce it into `[0, N)`. Returns the result handle, or
/// `None` to run the transcription.
fn fast_montgomery(ar: &mut Arena, a: H, b: H, n: H, len: i32, inv: i64, product: H) -> Option<H> {
    let (Some(as_), Some(bs), Some(ns)) = (a, b, n) else {
        return None;
    };
    if !fast_paths_on() || len < 2 || len % 2 != 0 {
        return None;
    }
    let l = len as usize;
    if l > ar.arrs[as_].len() || l > ar.arrs[bs].len() || l > ar.arrs[ns].len() {
        return None;
    }
    let reuse = match product {
        None => false,
        Some(p) => {
            let pl = ar.arrs[p].len();
            if pl < 2 * l {
                false
            } else if pl == 2 * l && p != as_ && p != bs && p != ns {
                true
            } else {
                return None;
            }
        }
    };
    let nw = &ar.arrs[ns][..l];
    if (nw[l - 1] as u32).wrapping_mul(inv as u32) != u32::MAX {
        return None;
    }
    if !words_lt(&ar.arrs[as_][..l], nw) || !words_lt(&ar.arrs[bs][..l], nw) {
        return None;
    }
    let s = l / 2;
    let mut buf = vec![0u64; 6 * s];
    let (am, rest) = buf.split_at_mut(s);
    let (bm, rest) = rest.split_at_mut(s);
    let (mm, rest) = rest.split_at_mut(s);
    let (t, out) = rest.split_at_mut(2 * s);
    be_to_limbs(&ar.arrs[as_][..l], am);
    be_to_limbs(nw, mm);
    if as_ == bs {
        sqr_limbs(am, t);
    } else {
        be_to_limbs(&ar.arrs[bs][..l], bm);
        mul_limbs(am, bm, t);
    }
    redc_limbs(t, mm, neg_inv64(mm[0]), &mut out[..s]);
    let dst = if reuse { product } else { None };
    let slot = match dst {
        Some(p) => p,
        None => match ar.new_array(2 * len) {
            Ok(i) => i,
            Err(_) => return None,
        },
    };
    let pa = &mut ar.arrs[slot];
    limbs_to_be(&out[..s], &mut pa[..l]);
    for w in pa[l..].iter_mut() {
        *w = 0;
    }
    ar.dirty[slot] = true;
    Some(Some(slot))
}

// --- Entry points -----------------------------------------------------------

fn int_arg(v: Option<&Value>, name: &str) -> Result<i32, MethodCallFailed> {
    require_int(v, name)
}

/// `private static int[] implSquareToLen(int[] x, int len, int[] z, int zlen)`
fn native_impl_square_to_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut ar = Arena::new();
    let x = ar.arg(ctx, args.first())?;
    let len = int_arg(args.get(1), "len")?;
    let z = ar.arg(ctx, args.get(2))?;
    let zlen = int_arg(args.get(3), "zlen")?;
    let r = j_impl_square_to_len(&mut ar, x, len, z, zlen).map(Some);
    ar.finish(ctx, r)
}

/// `private static int[] implMultiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z)`
fn native_impl_multiply_to_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut ar = Arena::new();
    let x = ar.arg(ctx, args.first())?;
    let xlen = int_arg(args.get(1), "xlen")?;
    let y = ar.arg(ctx, args.get(2))?;
    let ylen = int_arg(args.get(3), "ylen")?;
    let z = ar.arg(ctx, args.get(4))?;
    let r = j_impl_multiply_to_len(&mut ar, x, xlen, y, ylen, z).map(Some);
    ar.finish(ctx, r)
}

/// `private static int[] implMontgomeryMultiply(int[] a, int[] b, int[] n, int len, long inv, int[] product)`
fn native_impl_montgomery_multiply(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut ar = Arena::new();
    let a = ar.arg(ctx, args.first())?;
    let b = ar.arg(ctx, args.get(1))?;
    let n = ar.arg(ctx, args.get(2))?;
    let len = int_arg(args.get(3), "len")?;
    let inv = require_long(args.get(4), "inv")?;
    let product = ar.arg(ctx, args.get(5))?;
    let r = j_impl_montgomery_multiply(&mut ar, a, b, n, len, inv, product).map(Some);
    ar.finish(ctx, r)
}

/// `private static int[] implMontgomerySquare(int[] a, int[] n, int len, long inv, int[] product)`
fn native_impl_montgomery_square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut ar = Arena::new();
    let a = ar.arg(ctx, args.first())?;
    let n = ar.arg(ctx, args.get(1))?;
    let len = int_arg(args.get(2), "len")?;
    let inv = require_long(args.get(3), "inv")?;
    let product = ar.arg(ctx, args.get(4))?;
    let r = j_impl_montgomery_square(&mut ar, a, n, len, inv, product).map(Some);
    ar.finish(ctx, r)
}

/// `private static int implMulAdd(int[] out, int[] in, int offset, int len, int k)`
fn native_impl_mul_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut ar = Arena::new();
    let out = ar.arg(ctx, args.first())?;
    let in_ = ar.arg(ctx, args.get(1))?;
    let offset = int_arg(args.get(2), "offset")?;
    let len = int_arg(args.get(3), "len")?;
    let k = int_arg(args.get(4), "k")?;
    let r = j_impl_mul_add(&mut ar, out, in_, offset, len, k);
    finish_int(ar, ctx, r)
}

/// `static int mulAdd(int[] out, int[] in, int offset, int len, int k)` — the
/// checked wrapper (`implMulAddCheck` + `implMulAdd`). Not an
/// `@IntrinsicCandidate` itself; registered because `implSquareToLen`'s and
/// `montReduce`'s bytecode call it once per row.
fn native_mul_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut ar = Arena::new();
    let out = ar.arg(ctx, args.first())?;
    let in_ = ar.arg(ctx, args.get(1))?;
    let offset = int_arg(args.get(2), "offset")?;
    let len = int_arg(args.get(3), "len")?;
    let k = int_arg(args.get(4), "k")?;
    let r = j_mul_add(&mut ar, out, in_, offset, len, k);
    finish_int(ar, ctx, r)
}

fn finish_int(ar: Arena, ctx: &mut dyn NativeContext, r: Result<i32, Jx>) -> MethodCallResult {
    match r {
        Ok(v) => {
            ar.finish(ctx, Ok(None))?;
            Ok(Some(Value::Int(v)))
        }
        Err(e) => ar.finish(ctx, Err(e)),
    }
}

/// `static void shiftLeftImplWorker(int[] newArr, int[] oldArr, int newIdx, int shiftCount, int numIter)`
fn native_shift_left_impl_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let new_arr = require_int_array(args.first(), "newArr")?;
    let old_arr = require_int_array(args.get(1), "oldArr")?;
    let new_idx = require_int(args.get(2), "newIdx")? as i64;
    let shift_count = require_int(args.get(3), "shiftCount")?;
    let num_iter = require_int(args.get(4), "numIter")? as i64;

    if new_idx < 0 || num_iter < 0 || shift_count < 0 || shift_count >= 32 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "invalid args: newIdx={new_idx} shiftCount={shift_count} numIter={num_iter}"
            ),
        }
        .into());
    }
    let new_idx = new_idx as usize;
    let num_iter = num_iter as usize;

    let new_len = ctx.array_length(new_arr);
    let old_len = ctx.array_length(old_arr);
    // The worker reads `oldArr[numIter]` on its last pass, so `numIter` equal
    // to the length is already out of range, and it writes `numIter` cells
    // from `newIdx`.
    //
    // Wave 100 L7: the first access out of range raises the array access's
    // AIOOBE, "Index i out of bounds for length n", naming the array it hit
    // (this said "Array index out of range: n"). Iteration k reads
    // `oldArr[k]`, `oldArr[k + 1]`, then stores `newArr[newIdx + k]`.
    if num_iter > 0 {
        let k_old = old_len.saturating_sub(1);
        let k_new = new_len.saturating_sub(new_idx);
        if k_old < num_iter && k_old <= k_new {
            let index = if old_len == 0 { 0 } else { old_len };
            return Err(bigint_aioobe(index, old_len));
        }
        if k_new < num_iter {
            return Err(bigint_aioobe(new_idx + k_new, new_len));
        }
    }

    // Materialise; if newArr is the same array as oldArr, we still need the
    // pre-shift snapshot to avoid a self-aliasing read-after-write hazard
    // when new_idx == 0.
    let old = read_int_array(ctx, old_arr)?;
    let mut new_buf = if std::ptr::eq(new_arr.as_ptr(), old_arr.as_ptr()) {
        old.clone()
    } else {
        read_int_array(ctx, new_arr)?
    };
    shift_left_impl_worker(&mut new_buf, &old, new_idx, shift_count as u32, num_iter);
    write_int_array(ctx, new_arr, &new_buf)?;
    Ok(None)
}

/// `static void shiftRightImplWorker(int[] newArr, int[] oldArr, int newIdx, int shiftCount, int numIter)`
fn native_shift_right_impl_worker(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let new_arr = require_int_array(args.first(), "newArr")?;
    let old_arr = require_int_array(args.get(1), "oldArr")?;
    let new_idx = require_int(args.get(2), "newIdx")? as i64;
    let shift_count = require_int(args.get(3), "shiftCount")?;
    let num_iter = require_int(args.get(4), "numIter")? as i64;

    if new_idx < 0 || num_iter < 0 || shift_count < 0 || shift_count >= 32 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "invalid args: newIdx={new_idx} shiftCount={shift_count} numIter={num_iter}"
            ),
        }
        .into());
    }
    let new_idx = new_idx as usize;
    let num_iter = num_iter as usize;

    let new_len = ctx.array_length(new_arr);
    let old_len = ctx.array_length(old_arr);
    // Reads `oldArr[numIter]`; writes down from `newArr[numIter]`, or from
    // `newArr[numIter - 1]` when `newIdx` is zero.
    let top = if new_idx == 0 {
        num_iter.saturating_sub(1)
    } else {
        num_iter
    };
    // Wave 100 L7: the first iteration reads `oldArr[numIter]` before it
    // stores `newArr[top]`; each raises the array access's own AIOOBE text.
    if num_iter > 0 && num_iter >= old_len {
        return Err(bigint_aioobe(num_iter, old_len));
    }
    if num_iter > 0 && top >= new_len {
        return Err(bigint_aioobe(top, new_len));
    }

    let old = read_int_array(ctx, old_arr)?;
    let mut new_buf = if std::ptr::eq(new_arr.as_ptr(), old_arr.as_ptr()) {
        old.clone()
    } else {
        read_int_array(ctx, new_arr)?
    };
    shift_right_impl_worker(&mut new_buf, &old, new_idx, shift_count as u32, num_iter);
    write_int_array(ctx, new_arr, &new_buf)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration entry-point.
// ---------------------------------------------------------------------------

/// T19_H13_BIGINTEGER_INTRINSICS — register HotSpot-equivalent native
/// implementations of every `@IntrinsicCandidate` method on
/// `java.math.BigInteger`. These override the JDK's interpreted fallback
/// bytecode, eliminating the seconds-long spin KC16 saw inside
/// `BigInteger.<clinit>` / `SunJCE` parameter validation.
pub fn register_biginteger_intrinsics(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let bi = "java/math/BigInteger";
    // implSquareToLen — squareToLen's intrinsic body.
    registry.register(
        bi,
        "implSquareToLen",
        "([II[II)[I",
        native_impl_square_to_len,
    );
    // shiftLeftImplWorker / shiftRightImplWorker — primitive*Shift's body.
    registry.register(
        bi,
        "shiftLeftImplWorker",
        "([I[IIII)V",
        native_shift_left_impl_worker,
    );
    registry.register(
        bi,
        "shiftRightImplWorker",
        "([I[IIII)V",
        native_shift_right_impl_worker,
    );
    // implMulAdd / mulAdd — used inside implSquareToLen's pass 2.
    registry.register(bi, "implMulAdd", "([I[IIII)I", native_impl_mul_add);
    registry.register(bi, "mulAdd", "([I[IIII)I", native_mul_add);
    // implMultiplyToLen — the grade-school multiply kernel behind
    // `BigInteger.multiply` for every operand below KARATSUBA_THRESHOLD, which
    // covers every RSA/DSA/DH/GOST modulus in use.
    registry.register(
        bi,
        "implMultiplyToLen",
        "([II[II[I)[I",
        native_impl_multiply_to_len,
    );
    // implMontgomeryMultiply / implMontgomerySquare — the two kernels
    // `oddModPow` spends essentially all of its time in, and therefore the two
    // that decide how long RSA key generation, DSA/GOST parameter generation
    // and every Miller-Rabin round take. Registered last of the seven so the
    // list reads in the order `BigInteger.java` declares them.
    registry.register(
        bi,
        "implMontgomeryMultiply",
        "([I[I[IIJ[I)[I",
        native_impl_montgomery_multiply,
    );
    registry.register(
        bi,
        "implMontgomerySquare",
        "([I[IIJ[I)[I",
        native_impl_montgomery_square,
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests — unit-tested against a Vec<i32>-backed mock. The same code paths
// drive the heap-array native entry-points.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::Value;

    fn alloc_int_arr(ctx: &mut dyn NativeContext, vals: &[i32]) -> ObjectRef {
        let arr = ctx.new_array(ArrayElementType::Int, vals.len());
        for (i, v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*v));
        }
        arr
    }

    fn read_arr(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i32> {
        let n = ctx.array_length(arr);
        (0..n)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v,
                _ => panic!("expected Int"),
            })
            .collect()
    }

    /// Naive schoolbook multiplication of two big-endian magnitudes
    /// represented as `int[]`. Used as ground truth in tests. The JDK's
    /// algorithm yields exactly this output for the squaring case
    /// (`y == x`).
    ///
    /// This is an O(n^2) reference implementation — independent code path
    /// from the implSquareToLen we're testing, so any algorithmic drift
    /// in either is caught.
    fn naive_mul(x: &[i32], y: &[i32]) -> Vec<i32> {
        let xn = x.len();
        let yn = y.len();
        let n = xn + yn;
        // out[0] is the most-significant limb (big-endian).
        let mut out = vec![0u32; n];
        for i in (0..xn).rev() {
            let xi = x[i] as u32 as u64;
            let mut carry: u64 = 0;
            for j in (0..yn).rev() {
                let pos = i + j + 1;
                let p = xi * (y[j] as u32 as u64) + (out[pos] as u64) + carry;
                out[pos] = p as u32;
                carry = p >> 32;
            }
            // Propagate carry into out[i] and possibly beyond.
            let mut k = i;
            let mut c = carry;
            while c != 0 {
                let p = (out[k] as u64) + c;
                out[k] = p as u32;
                c = p >> 32;
                if k == 0 {
                    break;
                }
                k -= 1;
            }
        }
        out.into_iter().map(|w| w as i32).collect()
    }

    fn ref_square(x: &[i32]) -> Vec<i32> {
        naive_mul(x, x)
    }

    /// Interpret a big-endian `int[]` magnitude as a `u128`. Only used by the
    /// Montgomery tests below, which deliberately stay at two 32-bit limbs so
    /// every intermediate fits and the reference arithmetic is plain `u128` —
    /// an independent code path from everything under test.
    fn as_u128(v: &[i32]) -> u128 {
        let mut acc: u128 = 0;
        for &w in v {
            acc = (acc << 32) | (w as u32) as u128;
        }
        acc
    }

    /// `-m^-1 mod 2^32` for the low word of a big-endian magnitude — the `inv`
    /// that `montReduce` consumes, by Newton iteration (the same shape as
    /// `MutableBigInteger.inverseMod64`, one word narrower).
    fn neg_inv32(m: &[i32]) -> i32 {
        let m0 = m[m.len() - 1] as u32;
        assert_eq!(m0 & 1, 1, "Montgomery needs an odd modulus");
        let mut t: u32 = 1;
        for _ in 0..5 {
            t = t.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(t)));
        }
        assert_eq!(m0.wrapping_mul(t), 1);
        (t as i32).wrapping_neg()
    }

    // --- implMultiplyToLen ---------------------------------------------

    #[test]
    fn impl_multiply_to_len_matches_the_naive_reference() {
        let cases: &[(&[i32], &[i32])] = &[
            (&[0x1234_5678], &[0x7FFF_FFFF]),
            (&[-1], &[-1]),
            (&[0, 0], &[0x1234_5678, -1, 5]),
            (
                &[0x7FFF_FFFF, -1, -1, -2],
                &[0x1234_5678, -1698898192, 0x0F0F_0F0F, -252645136, 1],
            ),
        ];
        for (x, y) in cases {
            let mut z = vec![0i32; x.len() + y.len()];
            impl_multiply_to_len(x, x.len() as i32, y, y.len() as i32, &mut z).unwrap();
            assert_eq!(z, naive_mul(x, y), "x={x:?} y={y:?}");
        }
    }

    #[test]
    fn impl_multiply_to_len_leaves_words_above_the_product_untouched() {
        // `montReduce` indexes from `z.len()`, so a wide buffer's tail is
        // load-bearing, not slack. MEASURED on HotSpot 25.0.3+9 as the
        // `MTL widez` row of `apps/probes/L2IntrinsicProbe.java`.
        let x: &[i32] = &[0x7FFF_FFFF, -1, -1, -2];
        let y: &[i32] = &[0x1234_5678, -1698898192, 0x0F0F_0F0F, -252645136, 1];
        let mut z = vec![0x5A5A_5A5Au32 as i32; 12];
        impl_multiply_to_len(x, 4, y, 5, &mut z).unwrap();
        assert_eq!(&z[..9], &naive_mul(x, y)[..]);
        assert_eq!(&z[9..], &[0x5A5A_5A5Au32 as i32; 3]);
    }

    #[test]
    fn impl_multiply_to_len_reports_the_index_java_would_throw_on() {
        // xlen == 0: the JDK's `z[xstart]` is `z[-1]`, which throws rather
        // than doing nothing. A `usize` signature could not express this.
        let mut z: Vec<i32> = Vec::new();
        assert_eq!(impl_multiply_to_len(&[], 0, &[], 0, &mut z), Err(-1));
        // xlen past the operand: `x[xstart]` is the first bad read.
        let mut z = vec![0i32; 8];
        assert_eq!(impl_multiply_to_len(&[1, 2], 4, &[1, 2], 2, &mut z), Err(3));
    }

    // --- montReduce and its helpers ------------------------------------

    #[test]
    fn sub_n_returns_the_jdk_borrow() {
        let mut a = vec![0i32, 5];
        assert_eq!(sub_n(&mut a, &[0, 3], 2), 0);
        assert_eq!(a, vec![0, 2]);
        let mut a = vec![0i32, 3];
        assert_eq!(sub_n(&mut a, &[0, 5], 2), -1, "borrow out is -1, not 1");
        assert_eq!(a, vec![-1, -2]);
    }

    #[test]
    fn int_array_cmp_to_len_is_unsigned() {
        // -1 is 0xFFFFFFFF, i.e. the LARGEST word, not the smallest.
        assert_eq!(int_array_cmp_to_len(&[-1], &[1], 1), 1);
        assert_eq!(int_array_cmp_to_len(&[1], &[-1], 1), -1);
        assert_eq!(int_array_cmp_to_len(&[1, -1], &[1, -1], 2), 0);
    }

    #[test]
    fn add_one_carry_reports_the_carry_out() {
        // A carry that stays inside the number.
        let mut a = vec![0i32, 0, 0, -1];
        assert_eq!(add_one_carry(&mut a, 0, 3, 1), 0);
        // A carry that runs off the top: every word above the write is
        // 0xFFFF_FFFF, so the increment loop walks past index 0.
        let mut a = vec![-1i32, -1];
        assert_eq!(add_one_carry(&mut a, 0, 1, 1), 1);
    }

    /// Montgomery reduction has to leave `x * R^-1 mod m` in the high half,
    /// fully reduced into `[0, m)`. Verified against `u128` arithmetic at two
    /// limbs, where `R = 2^64` and every intermediate fits.
    #[test]
    fn mont_reduce_computes_x_times_r_inverse_mod_m() {
        let m: Vec<i32> = vec![0x4321_0ABD, 0x1357_9BDF];
        let len = m.len();
        let inv = neg_inv32(&m);
        let m_u = as_u128(&m);
        let r_inv = {
            // R^-1 mod m, by extended Euclid on u128.
            let (mut old_r, mut r) = ((1u128 << 64) % m_u, m_u);
            let (mut old_s, mut s) = (1i128, 0i128);
            while r != 0 {
                let q = old_r / r;
                let nr = old_r - q * r;
                old_r = r;
                r = nr;
                let ns = old_s - (q as i128) * s;
                old_s = s;
                s = ns;
            }
            assert_eq!(old_r, 1, "R must be invertible mod m");
            (((old_s % m_u as i128) + m_u as i128) % m_u as i128) as u128
        };

        for (x, y) in [
            (
                vec![0x0123_4567i32, -1985229329],
                vec![0x0FED_CBA9i32, 0x7654_3210],
            ),
            (vec![0i32, 0], vec![0x0FED_CBA9i32, 0x7654_3210]),
            (
                vec![0x4321_0ABDi32, 0x1357_9BDE],
                vec![0x4321_0ABDi32, 0x1357_9BDE],
            ),
        ] {
            let mut prod = vec![0i32; 2 * len];
            impl_multiply_to_len(&x, len as i32, &y, len as i32, &mut prod).unwrap();
            let full = as_u128(&prod);
            mont_reduce(&mut prod, &m, len, inv);
            let got = as_u128(&prod[..len]);
            let want = (full % m_u) * r_inv % m_u;
            assert_eq!(got, want, "montReduce({x:?} * {y:?}) mod {m:?}");
            assert!(got < m_u, "the residue must be fully reduced into [0, m)");
            assert_eq!(
                &prod[len..],
                &vec![0i32; len][..],
                "the low half must be zeroed by the reduction"
            );
        }
    }

    // 1) Trivial single-word — the simplest possible case.
    #[test]
    fn impl_square_to_len_single_word() {
        let x = vec![0x1234_5678i32];
        let mut z = vec![0i32; 2];
        impl_square_to_len(&x, 1, &mut z, 2);
        assert_eq!(z, ref_square(&x));
    }

    // 2) 2 limbs (64-bit operand).
    #[test]
    fn impl_square_to_len_two_words() {
        let x = vec![0x0000_0001i32, 0xFFFF_FFFFu32 as i32];
        let mut z = vec![0i32; 4];
        impl_square_to_len(&x, 2, &mut z, 4);
        assert_eq!(z, ref_square(&x));
    }

    // 3) 4 limbs (128-bit operand) — first non-trivial inner-loop pass.
    #[test]
    fn impl_square_to_len_four_words() {
        let x = vec![
            0x1234_5678i32,
            0x9ABC_DEF0u32 as i32,
            0x1111_2222i32,
            0x3333_4444i32,
        ];
        let mut z = vec![0i32; 8];
        impl_square_to_len(&x, 4, &mut z, 8);
        assert_eq!(z, ref_square(&x));
    }

    // 4) 32 limbs (1024-bit operand) — exercises the full diagonal +
    //    off-diagonal path the way SunJCE's BigInteger uses it.
    #[test]
    fn impl_square_to_len_thirty_two_words() {
        // Pick a deterministic non-degenerate magnitude.
        let x: Vec<i32> = (0..32u32)
            .map(|i| 0x4000_0001u32.wrapping_add(i.wrapping_mul(0x1357_BDF1)) as i32)
            .collect();
        let mut z = vec![0i32; 64];
        impl_square_to_len(&x, 32, &mut z, 64);
        let expected = ref_square(&x);
        assert_eq!(z, expected, "1024-bit implSquareToLen must equal naive_mul");
    }

    // 5) 64 limbs (2048-bit operand) — the actual KC16 trigger size.
    #[test]
    fn impl_square_to_len_2048_bit() {
        let x: Vec<i32> = (0..64u32)
            .map(|i| 0xCAFE_BABEu32.wrapping_add(i.wrapping_mul(0x0123_4567)) as i32)
            .collect();
        let mut z = vec![0i32; 128];
        impl_square_to_len(&x, 64, &mut z, 128);
        let expected = ref_square(&x);
        assert_eq!(z, expected, "2048-bit implSquareToLen must equal naive_mul");
    }

    // 6) shiftLeftImplWorker - n=1 base case covers KC16 hot path.
    //
    //    `numIter` is 3, not 4, and that is the CONTRACT rather than a
    //    shortened loop: `primitiveLeftShift` passes `len - 1` because it
    //    patches `a[len - 1] <<= n` itself, and the worker reads
    //    `oldArr[numIter]`, so `numIter == old.len()` would be out of range.
    //    This test used to pass 4 against a 4-word array and assert the top
    //    word stayed zero - which is also what an off-by-one in the worker
    //    produces, so it could not tell the two apart.
    #[test]
    fn shift_left_impl_worker_n1() {
        let mut new_buf = vec![0i32; 4];
        let old = vec![
            0x1111_1111i32,
            0x2222_2222i32,
            0x3333_3333i32,
            0x4444_4444i32,
        ];
        shift_left_impl_worker(&mut new_buf, &old, 0, 1, 3);
        // new_buf[i] = (old[i]<<1) | (old[i+1]>>>31)
        let expected = vec![
            ((old[0] as u32) << 1 | ((old[1] as u32) >> 31)) as i32,
            ((old[1] as u32) << 1 | ((old[2] as u32) >> 31)) as i32,
            ((old[2] as u32) << 1 | ((old[3] as u32) >> 31)) as i32,
            0i32, // patched by primitiveLeftShift, which is why numIter is 3
        ];
        assert_eq!(new_buf, expected);
    }

    // 7) shiftLeftImplWorker — n=15 (mid-range).
    #[test]
    fn shift_left_impl_worker_n15() {
        let mut new_buf = vec![0i32; 3];
        let old = vec![0x1234_5678i32, 0x9ABC_DEF0u32 as i32, 0x1111_2222i32];
        shift_left_impl_worker(&mut new_buf, &old, 0, 15, 2);
        let expected = vec![
            ((old[0] as u32) << 15 | ((old[1] as u32) >> 17)) as i32,
            ((old[1] as u32) << 15 | ((old[2] as u32) >> 17)) as i32,
            0i32,
        ];
        assert_eq!(new_buf, expected);
    }

    // 6b) Both workers, against rows MEASURED on HotSpot 25.0.4+7 rather than
    //     recomputed from this file's own arithmetic.
    //
    //     A test that rebuilds its expectation from the same expression the
    //     implementation uses agrees with the implementation by construction;
    //     tests 6 and 7 above do exactly that, which is half of why the
    //     off-by-one survived them. These literals come from running
    //     `apps/probes/L2IntrinsicProbe.java` on HotSpot, which calls the two
    //     methods reflectively so nothing else is in the answer.
    //
    //     The magnitude below is (2^127 - 2)'s - the value `RJdkSecurity`
    //     asserts on when `java/math/BigInteger` yields to real bytecode.
    #[test]
    fn the_shift_workers_match_hotspot_on_the_m127_magnitude() {
        let m127 = vec![
            0x7FFF_FFFFi32,
            0xFFFF_FFFFu32 as i32,
            0xFFFF_FFFFu32 as i32,
            0xFFFF_FFFEu32 as i32,
        ];

        // shiftRightImplWorker(newArr, m127, newIdx=1, shiftCount=1, numIter=3)
        let mut r1 = vec![0i32; 4];
        shift_right_impl_worker(&mut r1, &m127, 1, 1, 3);
        assert_eq!(r1, vec![0, -1, -1, -1], "right worker, newIdx = 1");

        // ...and with newIdx = 0, where OpenJDK starts at numIter - 1.
        let mut r0 = vec![0i32; 4];
        shift_right_impl_worker(&mut r0, &m127, 0, 1, 3);
        assert_eq!(r0, vec![-1, -1, -1, 0], "right worker, newIdx = 0");

        // numIter = 1 is ONE pass, not zero. The old body returned early for
        // `num_iter < 2` and wrote nothing at all.
        let mut r_one = vec![0i32; 4];
        shift_right_impl_worker(&mut r_one, &m127, 1, 1, 1);
        assert_eq!(r_one, vec![0, -1, 0, 0], "right worker, numIter = 1");

        let mut l_one = vec![0i32; 4];
        shift_left_impl_worker(&mut l_one, &m127, 0, 1, 1);
        assert_eq!(l_one, vec![-1, 0, 0, 0], "left worker, numIter = 1");

        // numIter = 0 writes nothing on both sides, and must not panic.
        let mut r_zero = vec![0i32; 4];
        shift_right_impl_worker(&mut r_zero, &m127, 1, 1, 0);
        assert_eq!(r_zero, vec![0, 0, 0, 0], "right worker, numIter = 0");
        let mut l_zero = vec![0i32; 4];
        shift_left_impl_worker(&mut l_zero, &m127, 0, 1, 0);
        assert_eq!(l_zero, vec![0, 0, 0, 0], "left worker, numIter = 0");
    }

    // 6c) An out-of-contract call declines instead of panicking.
    //
    //     A panic inside a native aborts the whole VM, so the pure helpers
    //     bounds-check and return; the entry points raise the AIOOBE that Java
    //     would. Reaching this is a caller bug either way - the point is which
    //     failure the operator gets.
    #[test]
    fn an_undersized_array_declines_rather_than_panicking() {
        let short = vec![1i32, 2, 3];
        let mut out = vec![0i32; 3];
        shift_left_impl_worker(&mut out, &short, 0, 1, 3); // would read short[3]
        assert_eq!(out, vec![0, 0, 0]);
        let mut out2 = vec![0i32; 3];
        shift_right_impl_worker(&mut out2, &short, 1, 1, 3); // would read short[3]
        assert_eq!(out2, vec![0, 0, 0]);
    }

    // 8) primitive_left_shift_inplace — n=1 in-place, full chain.
    #[test]
    fn primitive_left_shift_inplace_n1() {
        let mut buf = vec![0x8000_0000u32 as i32, 0x4000_0000i32, 0x2000_0000i32];
        primitive_left_shift_inplace(&mut buf, 3, 1);
        // a[0] = (a[0]<<1) | (a[1]>>>31) = 0 | 0 = 0
        // a[1] = (a[1]<<1) | (a[2]>>>31) = 0x80000000 | 0 = 0x80000000
        // a[2] <<= 1 => 0x40000000
        assert_eq!(buf, vec![0i32, 0x8000_0000u32 as i32, 0x4000_0000i32]);
    }

    // 9) mul_add — single-iteration baseline. JDK contract: writes start
    //    at `out[out.len() - offset - 1]` and walk down. Here offset=1,
    //    out.len()=2 → writes start at out[0].
    #[test]
    fn mul_add_basic() {
        let in_ = vec![0x0000_0007i32];
        let mut out = vec![0i32, 0i32];
        let carry = mul_add(&mut out, &in_, 1, 1, 5);
        // out[0] += in_[0] * 5 = 7 * 5 = 35
        assert_eq!(out, vec![35, 0]);
        assert_eq!(carry, 0);
    }

    // 10) mul_add — produces a 32-bit carry. With offset=1, out.len()=2 the
    //    write lands at out[0].
    #[test]
    fn mul_add_carry() {
        let in_ = vec![0xFFFF_FFFFu32 as i32];
        // out[0] starts as 0xFFFFFFFF; we add in[0]*k + out[0] + carry there.
        let mut out = vec![0xFFFF_FFFFu32 as i32, 0i32];
        let carry = mul_add(&mut out, &in_, 1, 1, 0x0000_0002);
        // 0xFFFFFFFF * 2 = 0x1_FFFFFFFE
        // + 0xFFFFFFFF = 0x2_FFFFFFFD
        // out[0] = 0xFFFFFFFD, carry = 2
        assert_eq!(out[0] as u32, 0xFFFF_FFFD);
        assert_eq!(carry as u32, 0x2);
    }

    // 11) Empty array — len=0 must early-return on primitive_left_shift_inplace.
    #[test]
    fn primitive_left_shift_inplace_empty() {
        let mut buf: Vec<i32> = vec![];
        primitive_left_shift_inplace(&mut buf, 0, 1);
        assert_eq!(buf, Vec::<i32>::new());
    }

    // 12) shift count 0 is a no-op.
    #[test]
    fn primitive_left_shift_inplace_n0() {
        let mut buf = vec![1i32, 2, 3];
        primitive_left_shift_inplace(&mut buf, 3, 0);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    // 13) Heap-array native entry-point — implSquareToLen 4-word case.
    #[test]
    fn native_impl_square_to_len_dispatch() {
        let mut ctx = mock_ctx();
        let x_vals = vec![
            0x1234_5678i32,
            0x9ABC_DEF0u32 as i32,
            0x1111_2222i32,
            0x3333_4444i32,
        ];
        let x = alloc_int_arr(&mut ctx, &x_vals);
        let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
        let r = native_impl_square_to_len(
            &mut ctx,
            &[
                Value::Object(Some(x)),
                Value::Int(4),
                Value::Object(Some(z)),
                Value::Int(8),
            ],
        )
        .expect("ok");
        match r {
            Some(Value::Object(Some(returned))) => {
                assert_eq!(read_arr(&ctx, returned), ref_square(&x_vals));
            }
            other => panic!("expected returned int[], got {other:?}"),
        }
    }

    // 14) Heap-array native entry-point — invalid len rejected with IAE.
    #[test]
    fn native_impl_square_to_len_invalid_len() {
        let mut ctx = mock_ctx();
        let x = alloc_int_arr(&mut ctx, &[1, 2, 3, 4]);
        let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
        let r = native_impl_square_to_len(
            &mut ctx,
            &[
                Value::Object(Some(x)),
                Value::Int(0), // invalid: len < 1
                Value::Object(Some(z)),
                Value::Int(8),
            ],
        );
        assert!(r.is_err(), "len < 1 must throw IllegalArgumentException");
    }

    // 15) Heap-array native entry-point — len > x.length rejected.
    #[test]
    fn native_impl_square_to_len_len_oob() {
        let mut ctx = mock_ctx();
        let x = alloc_int_arr(&mut ctx, &[1, 2]);
        let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
        let r = native_impl_square_to_len(
            &mut ctx,
            &[
                Value::Object(Some(x)),
                Value::Int(99), // len > x.length=2
                Value::Object(Some(z)),
                Value::Int(8),
            ],
        );
        assert!(r.is_err(), "len > x.length must throw");
    }

    // 16) Native shiftLeftImplWorker dispatch — verifies heap path matches
    //     the in-memory algorithm.
    #[test]
    fn native_shift_left_impl_worker_dispatch() {
        let mut ctx = mock_ctx();
        let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
        let old = vec![0x1234_5678i32, 0x9ABC_DEF0u32 as i32, 0x1111_2222i32];
        let old_arr = alloc_int_arr(&mut ctx, &old);
        let r = native_shift_left_impl_worker(
            &mut ctx,
            &[
                Value::Object(Some(new_buf)),
                Value::Object(Some(old_arr)),
                Value::Int(0),
                Value::Int(15),
                Value::Int(2),
            ],
        )
        .expect("ok");
        assert_eq!(r, None);
        let result = read_arr(&ctx, new_buf);
        let expected = vec![
            ((old[0] as u32) << 15 | ((old[1] as u32) >> 17)) as i32,
            ((old[1] as u32) << 15 | ((old[2] as u32) >> 17)) as i32,
            0i32,
        ];
        assert_eq!(result, expected);
    }

    // 16b) The heap entry points reject `numIter == oldArr.length`, which the
    //      corrected worker would read one past.
    #[test]
    fn native_shift_worker_rejects_num_iter_at_the_array_length() {
        let mut ctx = mock_ctx();
        let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
        let old_arr = alloc_int_arr(&mut ctx, &vec![1i32, 2, 3]);
        let args = [
            Value::Object(Some(new_buf)),
            Value::Object(Some(old_arr)),
            Value::Int(0),
            Value::Int(1),
            Value::Int(3),
        ];
        assert!(
            native_shift_left_impl_worker(&mut ctx, &args).is_err(),
            "left: numIter == oldArr.length must throw"
        );
        assert!(
            native_shift_right_impl_worker(&mut ctx, &args).is_err(),
            "right: numIter == oldArr.length must throw"
        );
    }

    // 17) Native shiftLeftImplWorker — out-of-range shift count rejected.
    #[test]
    fn native_shift_left_impl_worker_bad_shift() {
        let mut ctx = mock_ctx();
        let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
        let old_arr = alloc_int_arr(&mut ctx, &vec![1i32, 2, 3]);
        let r = native_shift_left_impl_worker(
            &mut ctx,
            &[
                Value::Object(Some(new_buf)),
                Value::Object(Some(old_arr)),
                Value::Int(0),
                Value::Int(40), // bad: >=32
                Value::Int(3),
            ],
        );
        assert!(r.is_err(), "shiftCount >= 32 must throw");
    }

    // 18) Native implMulAdd dispatch — verifies the carry result.
    //     With offset=1, out.length=2 the JDK rewrites offset to 0; the
    //     write lands at out[0].
    #[test]
    fn native_impl_mul_add_carry() {
        let mut ctx = mock_ctx();
        let in_arr = alloc_int_arr(&mut ctx, &[0xFFFF_FFFFu32 as i32]);
        let out_arr = alloc_int_arr(&mut ctx, &[0xFFFF_FFFFu32 as i32, 0i32]);
        let r = native_impl_mul_add(
            &mut ctx,
            &[
                Value::Object(Some(out_arr)),
                Value::Object(Some(in_arr)),
                Value::Int(1),
                Value::Int(1),
                Value::Int(2),
            ],
        )
        .expect("ok");
        // carry = 2
        assert_eq!(r, Some(Value::Int(2)));
        let result = read_arr(&ctx, out_arr);
        assert_eq!(result[0] as u32, 0xFFFF_FFFD);
    }

    // Round 13 wave 9: the windowed native equals the whole-array kernel, and
    // leaves every word outside the window alone.
    #[test]
    fn native_impl_mul_add_window_matches_whole_array_kernel() {
        let out0: Vec<i32> = (0..8)
            .map(|i| (0x9E37_79B9u32.wrapping_mul(i + 1)) as i32)
            .collect();
        let inp: Vec<i32> = (0..5)
            .map(|i| (0x7F4A_7C15u32.wrapping_mul(i + 3)) as i32)
            .collect();
        for (offset, len) in [(2usize, 3usize), (0, 5), (3, 5), (7, 1), (4, 0)] {
            let mut want = out0.clone();
            let want_carry = mul_add(&mut want, &inp, offset, len, 0x1234_5679);
            let mut ctx = mock_ctx();
            let in_arr = alloc_int_arr(&mut ctx, &inp);
            let out_arr = alloc_int_arr(&mut ctx, &out0);
            let r = native_impl_mul_add(
                &mut ctx,
                &[
                    Value::Object(Some(out_arr)),
                    Value::Object(Some(in_arr)),
                    Value::Int(offset as i32),
                    Value::Int(len as i32),
                    Value::Int(0x1234_5679),
                ],
            )
            .expect("ok");
            assert_eq!(r, Some(Value::Int(want_carry)), "offset {offset} len {len}");
            assert_eq!(read_arr(&ctx, out_arr), want, "offset {offset} len {len}");
        }
    }

    // 19) Null array argument rejected with NPE.
    #[test]
    fn native_impl_square_to_len_null_array() {
        let mut ctx = mock_ctx();
        let r = native_impl_square_to_len(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Int(1),
                Value::Object(None),
                Value::Int(2),
            ],
        );
        assert!(r.is_err(), "null array must throw NPE");
    }

    // 20) Registration smoke test — every native is wired.
    #[test]
    fn registers_all_intrinsics() {
        let mut reg = NativeMethodRegistry::new();
        register_biginteger_intrinsics(&mut reg);
        let bi = "java/math/BigInteger";
        assert!(reg.find(bi, "implSquareToLen", "([II[II)[I").is_some());
        assert!(reg.find(bi, "shiftLeftImplWorker", "([I[IIII)V").is_some());
        assert!(reg.find(bi, "shiftRightImplWorker", "([I[IIII)V").is_some());
        assert!(reg.find(bi, "implMulAdd", "([I[IIII)I").is_some());
        assert!(reg.find(bi, "mulAdd", "([I[IIII)I").is_some());
    }

    // --- SBR wave 5: the fast paths against the transcription ----------------

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn word(&mut self) -> i32 {
            match self.next() % 8 {
                0 => 0,
                1 => -1,
                _ => self.next() as i32,
            }
        }
        fn words(&mut self, n: usize) -> Vec<i32> {
            (0..n).map(|_| self.word()).collect()
        }
    }

    fn arena(arrays: Vec<Vec<i32>>) -> Arena {
        let mut ar = Arena::new();
        for a in arrays {
            ar.dirty.push(false);
            ar.refs.push(None);
            ar.arrs.push(a);
        }
        ar
    }

    /// Run `f` twice on identical arenas — fast paths on, then transcription
    /// only — and require identical results and identical arrays.
    fn both<T: PartialEq + std::fmt::Debug>(
        arrays: Vec<Vec<i32>>,
        f: impl Fn(&mut Arena) -> Result<T, Jx>,
    ) -> (Result<T, Jx>, Vec<Vec<i32>>) {
        let mut fast = arena(arrays.clone());
        let rf = f(&mut fast);
        FORCE_TRANSCRIPTION.with(|c| c.set(true));
        let mut slow = arena(arrays);
        let rs = f(&mut slow);
        FORCE_TRANSCRIPTION.with(|c| c.set(false));
        assert_eq!(rf, rs, "result");
        assert_eq!(fast.arrs, slow.arrs, "arrays");
        (rf, fast.arrs)
    }

    fn odd_modulus(r: &mut Rng, len: usize) -> (Vec<i32>, i64) {
        let mut m = r.words(len);
        if m[0] == 0 {
            m[0] = 1;
        }
        m[len - 1] |= 1;
        let n0 = (m[len - 1] as u32 as u64) | ((m[len - 2] as u32 as u64) << 32);
        (m, neg_inv64(n0) as i64)
    }

    fn below(r: &mut Rng, m: &[i32]) -> Vec<i32> {
        let mut a = r.words(m.len());
        a[0] = ((a[0] as u32 as u64) % (m[0] as u32 as u64).max(1)) as i32;
        a
    }

    #[test]
    fn fast_montgomery_matches_the_java_body() {
        let mut r = Rng(0x1234_5678_9abc_def1);
        for len in (2..=66).step_by(2) {
            for shape in 0..4 {
                let (m, inv) = odd_modulus(&mut r, len);
                let a = below(&mut r, &m);
                let b = below(&mut r, &m);
                let prod = match shape {
                    0 => None,
                    1 => Some(vec![0x5a5a_5a5a; 2 * len]),
                    2 => Some(vec![7; len]), // short: Java allocates
                    _ => Some(vec![9; 2 * len]),
                };
                let mut arrays = vec![a, b, m];
                if let Some(p) = prod {
                    arrays.push(p);
                }
                let has_p = arrays.len() == 4;
                let (res, arrs) = both(arrays.clone(), |ar| {
                    let p = if has_p { Some(3) } else { None };
                    j_impl_montgomery_multiply(ar, Some(0), Some(1), Some(2), len as i32, inv, p)
                        .map(|h| h.map(|i| ar.arrs[i].clone()))
                });
                let out = res.expect("in-domain call").expect("non-null");
                assert_eq!(out.len(), 2 * len);
                assert!(out[len..].iter().all(|&w| w == 0), "low half zeroed");
                let _ = arrs;
                // Square, same shapes.
                let (res, _) = both(arrays, |ar| {
                    let p = if has_p { Some(3) } else { None };
                    j_impl_montgomery_square(ar, Some(0), Some(2), len as i32, inv, p)
                        .map(|h| h.map(|i| ar.arrs[i].clone()))
                });
                res.expect("in-domain square");
            }
        }
    }

    #[test]
    fn fast_multiply_and_square_match_the_java_body() {
        let mut r = Rng(0xdead_beef_cafe_f00d);
        for xl in 1..=40usize {
            let yl = 1 + (r.next() % 40) as usize;
            let x = r.words(xl);
            let y = r.words(yl);
            let z = vec![0x3c3c_3c3c; xl + yl + (r.next() % 3) as usize];
            let (res, arrs) = both(vec![x.clone(), y.clone(), z], |ar| {
                j_impl_multiply_to_len(ar, Some(0), xl as i32, Some(1), yl as i32, Some(2))
            });
            assert_eq!(res, Ok(Some(2)));
            assert_eq!(&arrs[2][..xl + yl], &naive_mul(&x, &y)[..]);
            let (res, arrs) = both(vec![x.clone(), vec![0; 2 * xl]], |ar| {
                j_impl_square_to_len(ar, Some(0), xl as i32, Some(1), 2 * xl as i32)
            });
            assert_eq!(res, Ok(Some(1)));
            assert_eq!(arrs[1], ref_square(&x));
            // mulAdd over the same operands.
            let off = (r.next() % 3) as i32;
            let (_res, _) = both(vec![r.words(xl + 3), x.clone()], |ar| {
                j_mul_add(ar, Some(0), Some(1), off, xl as i32, -7)
            });
        }
    }

    /// Exception texts measured on HotSpot 25.0.4+7 `-Xint` (the Java bodies,
    /// called reflectively; `java.base` carries parameter names).
    #[test]
    fn the_java_body_exceptions_carry_hotspot_texts() {
        let npe = |s: &str| Err::<Option<usize>, Jx>(Jx::Npe(Some(s.to_string())));
        let mut ar = arena(vec![vec![0; 1], vec![0; 2]]);
        assert_eq!(
            j_impl_multiply_to_len(&mut ar, None, 1, Some(0), 1, Some(1)),
            npe("Cannot load from int array because \"x\" is null")
        );
        let mut ar = arena(vec![vec![0; 1], vec![0; 2]]);
        assert_eq!(
            j_impl_multiply_to_len(&mut ar, Some(0), 1, Some(0), 1, None),
            npe("Cannot store to int array because \"z\" is null")
        );
        let mut ar = arena(vec![vec![0; 1], vec![0; 1], vec![0; 2]]);
        assert_eq!(
            j_impl_multiply_to_len(&mut ar, Some(0), 0, Some(1), 0, Some(2)),
            Err(Jx::Index {
                index: -1,
                length: 2
            })
        );
        let mut ar = arena(vec![vec![0; 2], vec![0; 2]]);
        assert_eq!(
            j_mul_add(&mut ar, Some(0), Some(1), -1, 1, 3),
            Err(Jx::Iae("input offset is invalid: -1".into()))
        );
        assert_eq!(
            j_mul_add(&mut ar, Some(0), Some(1), 5, 1, 3),
            Err(Jx::Iae("input offset is out of bound: 5 > 1".into()))
        );
        assert_eq!(
            j_mul_add(&mut ar, Some(0), None, 0, 1, 3),
            Err(Jx::Npe(Some(
                "Cannot read the array length because \"in\" is null".into()
            )))
        );
        assert_eq!(
            j_impl_mul_add(&mut ar, Some(0), Some(1), -1, 1, 3),
            Err(Jx::Index {
                index: 2,
                length: 2
            })
        );
        // implMontgomeryMultiply(null, ...) -> multiplyToLenCheck's requireNonNull.
        let mut ar = arena(vec![vec![0; 2], vec![1, 3]]);
        assert_eq!(
            j_impl_montgomery_multiply(&mut ar, None, Some(0), Some(1), 2, 1, None),
            Err(Jx::Npe(None))
        );
        // ... with mod null -> implMulAddCheck's `in.length`.
        let mut ar = arena(vec![vec![0; 2], vec![0; 2]]);
        assert_eq!(
            j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), None, 2, 1, None),
            Err(Jx::Npe(Some(
                "Cannot read the array length because \"in\" is null".into()
            )))
        );
        // ... len -2 -> new int[-4].
        let mut ar = arena(vec![vec![0; 2], vec![0; 2], vec![1, 3]]);
        assert_eq!(
            j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), Some(2), -2, 1, None),
            Err(Jx::Nase(-4))
        );
        // ... mod shorter than len.
        let mut ar = arena(vec![vec![0; 2], vec![0; 2], vec![1]]);
        assert_eq!(
            j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), Some(2), 2, 1, None),
            Err(Jx::Iae("input length is out of bound: 2 > 1".into()))
        );
        // implMontgomerySquare(null a) -> implSquareToLenChecks' `x.length`.
        let mut ar = arena(vec![vec![1, 3]]);
        assert_eq!(
            j_impl_montgomery_square(&mut ar, None, Some(0), 2, 1, None),
            Err(Jx::Npe(Some(
                "Cannot read the array length because \"x\" is null".into()
            )))
        );
    }

    /// A tiny modulus with operands far above it: the Java body's final
    /// `while (n >= mod) subN` loop would run ~2^32 times; the closed form
    /// must land where the loop does (checked against a direct `%`).
    #[test]
    fn mont_reduce_closed_form_matches_the_subtraction_loop() {
        // mod = [0, 3] (value 3), len 2, a = b = 2^48 - 1 (far above mod).
        let a = vec![0xFFFFi32, -1];
        let m = vec![0i32, 3];
        let inv = neg_inv64(3) as i64;
        let mut ar = arena(vec![a.clone(), m.clone()]);
        let out = j_impl_montgomery_multiply(&mut ar, Some(0), Some(0), Some(1), 2, inv, None)
            .expect("terminates")
            .expect("non-null");
        let got = &ar.arrs[out];
        // T = a^2 < 2^96; U = (T + Q*3) / 2^64 with Q = -T*3^-1 mod 2^64.
        let av: u128 = (1u128 << 48) - 1;
        let t: u128 = av * av;
        let q = (t as u64).wrapping_mul(neg_inv64(3)) as u128;
        let u = (t + q * 3) >> 64;
        let want = (u % 3) as u64;
        assert_eq!(got[0], 0);
        assert_eq!(got[1] as u32 as u64, want);
        assert_eq!(&got[2..], &[0, 0]);
    }
}

// Suppress unused warnings on helpers exported for symmetry with future
// intrinsic additions.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = LONG_MASK;
    let _ = read_int as fn(&dyn NativeContext, ObjectRef, usize, usize) -> _;
    let _ = write_int as fn(&dyn NativeContext, ObjectRef, usize, usize, i32) -> _;
    let _ = require_long as fn(Option<&Value>, &str) -> _;
}
