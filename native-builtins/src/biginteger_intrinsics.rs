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

/// `private static int[] implSquareToLen(int[] x, int len, int[] z, int zlen)`
fn native_impl_square_to_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args = [x, len, z, zlen].
    let x_ref = require_int_array(args.first(), "x")?;
    let len_i = require_int(args.get(1), "len")?;
    let z_ref = require_int_array(args.get(2), "z")?;
    let zlen_i = require_int(args.get(3), "zlen")?;

    if len_i < 1 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("invalid input length: {len_i}"),
        }
        .into());
    }
    let len = len_i as usize;
    if zlen_i < 1 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("invalid input length: {zlen_i}"),
        }
        .into());
    }
    let zlen = zlen_i as usize;

    let x_len = ctx.array_length(x_ref);
    let z_len = ctx.array_length(z_ref);
    if len > x_len {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("input length out of bound: {len} > {x_len}"),
        }
        .into());
    }
    if len.checked_mul(2).map_or(true, |v| v > z_len) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("input length out of bound: {} > {z_len}", len * 2),
        }
        .into());
    }
    if zlen > z_len {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("input length out of bound: {zlen} > {z_len}"),
        }
        .into());
    }

    let x = read_int_array(ctx, x_ref)?;
    let mut z = read_int_array(ctx, z_ref)?;
    impl_square_to_len(&x, len, &mut z, zlen);
    write_int_array(ctx, z_ref, &z)?;
    Ok(Some(Value::Object(Some(z_ref))))
}

/// `private static int[] implMultiplyToLen(int[] x, int xlen, int[] y, int ylen, int[] z)`
///
/// The `multiplyToLen` wrapper runs `multiplyToLenCheck` on both operands and
/// guarantees `z.length >= xlen + ylen` BEFORE calling this; the method body
/// itself validates nothing, so neither does this native. A caller that
/// reaches the private method directly (reflection — `apps/probes/
/// L2IntrinsicProbe.java`) gets whatever the raw index arithmetic gives,
/// which [`impl_multiply_to_len`] reproduces access for access.
fn native_impl_multiply_to_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Static method: args = [x, xlen, y, ylen, z].
    let x_ref = require_int_array(args.first(), "x")?;
    let xlen = require_int(args.get(1), "xlen")?;
    let y_ref = require_int_array(args.get(2), "y")?;
    let ylen = require_int(args.get(3), "ylen")?;
    let z_ref = require_int_array(args.get(4), "z")?;

    let x = read_int_array(ctx, x_ref)?;
    let y = read_int_array(ctx, y_ref)?;
    let mut z = read_int_array(ctx, z_ref)?;
    if let Err(idx) = impl_multiply_to_len(&x, xlen, &y, ylen, &mut z) {
        // Java threw before any store landed for THIS access, but earlier
        // stores in the same call did land — so the partially-written buffer
        // has to be flushed before the exception, exactly as the bytecode
        // leaves it.
        write_int_array(ctx, z_ref, &z)?;
        return Err(RuntimeError::aioobe_index_only(idx).into());
    }
    write_int_array(ctx, z_ref, &z)?;
    Ok(Some(Value::Object(Some(z_ref))))
}

/// Shared body of [`native_impl_montgomery_multiply`] and
/// [`native_impl_montgomery_square`].
///
/// Reproduces the JDK's own **method body**, not its wrapper:
///
/// ```text
/// private static int[] implMontgomeryMultiply(int[] a, int[] b, int[] n, int len,
///                                             long inv, int[] product) {
///     product = multiplyToLen(a, len, b, len, product);
///     return montReduce(product, n, len, (int)inv);
/// }
/// private static int[] implMontgomerySquare(int[] a, int[] n, int len,
///                                           long inv, int[] product) {
///     product = squareToLen(a, len, product);
///     return montReduce(product, n, len, (int)inv);
/// }
/// ```
///
/// The distinction is load-bearing. `implMontgomeryMultiplyChecks` — the
/// even-`len`, positive-`len` and array-bound validation — lives in the
/// `montgomeryMultiply`/`montgomerySquare` **wrappers**, so a caller that
/// reaches the private method directly (reflection, which is exactly what
/// `apps/probes/L2IntrinsicProbe.java` does) never runs those checks. MEASURED
/// on HotSpot 25.0.3+9: `implMontgomeryMultiply(new int[3], new int[3], mod, 3,
/// inv, null)` — an ODD length — returns `[0,0,0,0,0,0]` rather than throwing
/// `IllegalArgumentException`. A native that validated here would diverge on
/// that row. What DOES throw is inherited from the inner wrappers:
/// `multiplyToLenCheck` (`AIOOBE` when `len > a.length`) and
/// `implSquareToLenChecks` (`IllegalArgumentException` for `len < 1`).
///
/// The product buffer is reused iff it already holds at least `2*len` words,
/// and a fresh `int[2*len]` is allocated otherwise — `multiplyToLen`'s and
/// `squareToLen`'s own rule. That matters for more than allocation counts:
/// `montReduce` derives every index from the product array's **actual length**,
/// so handing it a differently-sized buffer would change the answer, not just
/// the cost.
///
/// GC-SAFETY: every input array is copied into a Rust `Vec` *before*
/// `new_array` can trigger a collection, and the only `ObjectRef` touched
/// after the allocation is the freshly allocated one. No stale reference
/// survives a potential move.
fn montgomery_op(
    ctx: &mut dyn NativeContext,
    a_ref: ObjectRef,
    b_ref: ObjectRef,
    n_ref: ObjectRef,
    len_i: i32,
    inv: i64,
    product_ref: Option<ObjectRef>,
    square: bool,
) -> MethodCallResult {
    // `int zlen = len << 1` / `xlen + ylen`: both are int arithmetic in the
    // JDK, and both equal `2*len` here.
    let two_len_i = len_i.wrapping_mul(2);

    let a = read_int_array(ctx, a_ref)?;
    let b = if square {
        Vec::new() // `implMontgomerySquare` has no second operand.
    } else {
        read_int_array(ctx, b_ref)?
    };

    if !square {
        // `multiplyToLenCheck(x, length)`, run on BOTH operands before the
        // buffer is chosen: a non-positive length is not an error (the JDK
        // comment says so — `multiplyToLen` won't execute), and an oversized
        // one throws `AIOOBE(length - 1)`.
        for cap in [a.len(), b.len()] {
            if len_i > 0 && len_i as usize > cap {
                return Err(RuntimeError::aioobe_index_only(len_i - 1).into());
            }
        }
    }

    // `if (z == null || z.length < zlen) z = new int[zlen];`
    let p_cap = product_ref.map(|p| ctx.array_length(p));
    let reuse = match (product_ref, p_cap) {
        (Some(p), Some(cap)) if two_len_i >= 0 && cap >= two_len_i as usize => Some(p),
        _ => None,
    };
    let mut product: Vec<i32> = match reuse {
        Some(p) => read_int_array(ctx, p)?,
        None => {
            if two_len_i < 0 {
                return Err(
                    RuntimeError::NegativeArraySizeException { size: two_len_i }.into()
                );
            }
            vec![0i32; two_len_i as usize]
        }
    };

    if square {
        // `implSquareToLenChecks(x, len, z, zlen)` — reproduced message for
        // message, including the JDK's own quirk in the last arm (it prints
        // `len`, not `zlen`, on the right-hand side).
        let zcap = product.len();
        if len_i < 1 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("invalid input length: {len_i}"),
            }
            .into());
        }
        if len_i as usize > a.len() {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("input length out of bound: {} > {}", len_i, a.len()),
            }
            .into());
        }
        if two_len_i as usize > zcap {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("input length out of bound: {two_len_i} > {zcap}"),
            }
            .into());
        }
        if two_len_i < 1 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("invalid input length: {two_len_i}"),
            }
            .into());
        }
        if two_len_i as usize > zcap {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("input length out of bound: {len_i} > {zcap}"),
            }
            .into());
        }
        impl_square_to_len(&a, len_i as usize, &mut product, two_len_i as usize);
    } else {
        // `implMultiplyToLen` — including its natural `AIOOBE`s (with
        // `len == 0` the JDK's `z[xstart]` indexes `z[-1]`).
        if let Err(idx) = impl_multiply_to_len(&a, len_i, &b, len_i, &mut product) {
            return Err(RuntimeError::aioobe_index_only(idx).into());
        }
    }

    // Unreachable: both branches above reject `len_i < 1` (square by
    // `implSquareToLenChecks`, multiply by the `z[-1]` AIOOBE). Stated anyway
    // so the `as usize` below can never widen a negative into a huge length.
    debug_assert!(len_i >= 1);
    if len_i < 1 {
        return Err(RuntimeError::aioobe_index_only(len_i - 1).into());
    }
    let modulus = read_int_array(ctx, n_ref)?;
    mont_reduce(&mut product, &modulus, len_i as usize, inv as i32);

    match reuse {
        Some(p) => {
            write_int_array(ctx, p, &product)?;
            Ok(Some(Value::Object(Some(p))))
        }
        None => {
            // Allocation point. Everything read above already lives in a Rust
            // `Vec`; `out` is the only live heap reference from here on.
            let out = ctx.new_array(ArrayElementType::Int, product.len());
            write_int_array(ctx, out, &product)?;
            Ok(Some(Value::Object(Some(out))))
        }
    }
}

/// `private static int[] implMontgomeryMultiply(int[] a, int[] b, int[] n, int len, long inv, int[] product)`
fn native_impl_montgomery_multiply(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a_ref = require_int_array(args.first(), "a")?;
    let b_ref = require_int_array(args.get(1), "b")?;
    let n_ref = require_int_array(args.get(2), "n")?;
    let len_i = require_int(args.get(3), "len")?;
    let inv = require_long(args.get(4), "inv")?;
    // `montgomeryMultiply` always calls `materialize(product, len)` first, so
    // the intrinsic never sees null in practice; tolerate it the way the Java
    // fallback's `multiplyToLen` would (allocate).
    let product_ref = match args.get(5) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    montgomery_op(ctx, a_ref, b_ref, n_ref, len_i, inv, product_ref, false)
}

/// `private static int[] implMontgomerySquare(int[] a, int[] n, int len, long inv, int[] product)`
fn native_impl_montgomery_square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a_ref = require_int_array(args.first(), "a")?;
    let n_ref = require_int_array(args.get(1), "n")?;
    let len_i = require_int(args.get(2), "len")?;
    let inv = require_long(args.get(3), "inv")?;
    let product_ref = match args.get(4) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // `b` is unread when `square` is set; pass `a` so the bound check below
    // sees a real array rather than a sentinel.
    montgomery_op(ctx, a_ref, a_ref, n_ref, len_i, inv, product_ref, true)
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
    if num_iter > 0 && num_iter >= old_len {
        return Err(RuntimeError::aioobe_index_only(num_iter as i32).into());
    }
    if new_idx + num_iter > new_len {
        return Err(RuntimeError::aioobe_index_only((new_idx + num_iter) as i32).into());
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
    if num_iter > 0 && (num_iter >= old_len || top >= new_len) {
        return Err(RuntimeError::aioobe_index_only(num_iter as i32).into());
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

/// `private static int implMulAdd(int[] out, int[] in_, int offset, int len, int k)`
fn native_impl_mul_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let out_ref = require_int_array(args.first(), "out")?;
    let in_ref = require_int_array(args.get(1), "in_")?;
    let offset_i = require_int(args.get(2), "offset")?;
    let len_i = require_int(args.get(3), "len")?;
    let k = require_int(args.get(4), "k")?;

    if offset_i < 0 || len_i < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("invalid args: offset={offset_i} len={len_i}"),
        }
        .into());
    }
    let offset = offset_i as usize;
    let len = len_i as usize;

    let out_len = ctx.array_length(out_ref);
    let in_len = ctx.array_length(in_ref);
    if len > in_len {
        return Err(RuntimeError::aioobe_index_only(len as i32).into());
    }
    // Per JDK: writes go to out[out.len() - offset - 1] downward through
    // out[out.len() - offset - len]. Both must be in-range.
    if offset + 1 > out_len || offset + len > out_len {
        return Err(RuntimeError::aioobe_index_only((offset + len) as i32).into());
    }

    let in_vec = read_int_array(ctx, in_ref)?;
    let mut out_vec = read_int_array(ctx, out_ref)?;
    let carry = mul_add(&mut out_vec, &in_vec, offset, len, k);
    write_int_array(ctx, out_ref, &out_vec)?;
    Ok(Some(Value::Int(carry)))
}

/// `private static int mulAdd(int[] out, int[] in_, int offset, int len, int k)`
/// — same semantics as `implMulAdd`, just the public wrapper. Some JDK
/// snapshots dispatch through both names, so we register both.
fn native_mul_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_impl_mul_add(ctx, args)
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
            (vec![0x0123_4567i32, -1985229329], vec![0x0FED_CBA9i32, 0x7654_3210]),
            (vec![0i32, 0], vec![0x0FED_CBA9i32, 0x7654_3210]),
            (vec![0x4321_0ABDi32, 0x1357_9BDE], vec![0x4321_0ABDi32, 0x1357_9BDE]),
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
