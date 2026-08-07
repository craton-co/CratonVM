// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.math.BigInteger` / `BigDecimal` intrinsics (magnitude arithmetic, modPow, scaling).
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

fn bi_alloc_mag_array(ctx: &mut dyn NativeContext, len: usize) -> ObjectRef {
    ctx.try_new_array(cratonvm_types::ArrayElementType::Int, len)
        .unwrap_or_else(|| ctx.new_array(cratonvm_types::ArrayElementType::Int, len))
}

/// RBIGDEC.1 — Resolve the real-JDK BigInteger field layout if available.
///
/// Returns `Some((signum_idx, mag_idx))` when the JDK class is loaded with the
/// real fields `signum:I` and `mag:[I`.  Returns `None` in synthetic-jdk mode
/// or before the class has been loaded — callers fall back to the legacy
/// 2-field synthetic layout (`BI_FIELD_VALUE` / `BI_FIELD_SIGNUM`).
pub(crate) fn bi_layout(ctx: &dyn NativeContext) -> Option<(usize, usize)> {
    let s = ctx.resolve_field_index("java/math/BigInteger", "signum")?;
    let m = ctx.resolve_field_index("java/math/BigInteger", "mag")?;
    Some((s, m))
}

/// Read a `BigInteger` instance and return its decimal string representation.
///
/// Two layouts are supported:
///   * Real-JDK layout (slot 0 = `signum:I`, slot 1 = `mag:[I`): we convert
///     the magnitude array (big-endian, base 2^32) to a decimal string and
///     prepend `-` if `signum < 0`.
///   * Synthetic-stub layout (slot 0 = `value:String`): we read the string
///     directly.
pub(crate) fn bi_read(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let signum = match ctx.get_field(this, sig_i) {
            Value::Int(s) => s,
            _ => 0,
        };
        if signum == 0 {
            return "0".to_string();
        }
        let mag = match ctx.get_field(this, mag_i) {
            Value::Object(Some(o)) => o,
            _ => return "0".to_string(),
        };
        let len = ctx.array_length(mag);
        if len == 0 {
            return "0".to_string();
        }
        let mut words: Vec<u32> = Vec::with_capacity(len);
        for i in 0..len {
            let w = match ctx.get_array_element(mag, i) {
                Value::Int(v) => v as u32,
                _ => 0,
            };
            words.push(w);
        }
        let abs = mag_words_to_decimal(&words);
        if signum < 0 {
            format!("-{}", abs)
        } else {
            abs
        }
    } else {
        match ctx.get_field(this, BI_FIELD_VALUE) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "0".to_string()),
            _ => "0".to_string(),
        }
    }
}

pub(crate) fn bi_alloc(ctx: &mut dyn NativeContext, value: &str) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/math/BigInteger", 2);
    // GC-SAFETY (use-after-move — mirrors the `bi_alloc_int` fix): `obj` is
    // freshly allocated and not yet reachable from any Java root. The
    // `new_array` / `create_string` allocations below can trigger a minor GC
    // that relocates `obj`; the bare local would then be STALE (resolving to a
    // reused java.lang.Object slot) and the subsequent `set_field` would corrupt
    // the heap — the H2 TestScript BigDecimal SEGV + `set_field` OOB flood. Pin
    // `obj` across the allocation and re-read the forwarded ref before writing.
    let h = ctx.pin_native_root(obj);
    let signum = if value.starts_with('-') {
        -1
    } else if value == "0" {
        0
    } else {
        1
    };
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        // Real-JDK layout: write signum + mag[].  This is the canonical
        // representation that bytecode reads via `getfield`.
        let mag_words = decimal_to_mag_words(value);
        let mag_arr = bi_alloc_mag_array(ctx, mag_words.len());
        let obj = ctx.read_native_pin(h, obj);
        for (i, w) in mag_words.iter().enumerate() {
            ctx.set_array_element(mag_arr, i, Value::Int(*w as i32));
        }
        ctx.set_field(obj, sig_i, Value::Int(signum));
        ctx.set_field(obj, mag_i, Value::Object(Some(mag_arr)));
        ctx.unpin_native_roots(h);
        obj
    } else {
        // Synthetic-stub fallback.
        let s = ctx.create_string(value);
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BI_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BI_FIELD_SIGNUM, Value::Int(signum));
        ctx.unpin_native_roots(h);
        obj
    }
}

/// Read a `BigInteger` instance directly into the limb-based [`crate::bigint::BigInt`]
/// — `O(words)`, with NO decimal conversion (unlike `bi_read`, which builds a
/// decimal string via `mag_words_to_decimal`). This is the fast read boundary
/// for the limb rewrite (step 3): the `mag:[I` field is big-endian base-2^32,
/// so we reverse it into little-endian limbs.
pub(crate) fn bi_read_int(ctx: &dyn NativeContext, this: ObjectRef) -> crate::bigint::BigInt {
    use crate::bigint::BigInt;
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let signum = match ctx.get_field(this, sig_i) {
            Value::Int(s) => s,
            _ => 0,
        };
        if signum == 0 {
            return BigInt::zero();
        }
        let mag = match ctx.get_field(this, mag_i) {
            Value::Object(Some(o)) => o,
            _ => return BigInt::zero(),
        };
        let len = ctx.array_length(mag);
        // big-endian array (index 0 = most significant) → little-endian limbs.
        let mut words: Vec<u32> = Vec::with_capacity(len);
        for i in (0..len).rev() {
            let w = match ctx.get_array_element(mag, i) {
                Value::Int(v) => v as u32,
                _ => 0,
            };
            words.push(w);
        }
        BigInt::from_le_words(signum < 0, words)
    } else {
        // Synthetic-stub fallback: parse the decimal string.
        let s = match ctx.get_field(this, BI_FIELD_VALUE) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "0".to_string()),
            _ => "0".to_string(),
        };
        BigInt::from_decimal(&s)
    }
}

/// Allocate a `BigInteger` from a limb-based [`crate::bigint::BigInt`] —
/// `O(words)`, writing `signum` + big-endian `mag:[I` directly with NO decimal
/// conversion (unlike `bi_alloc`, which goes through `decimal_to_mag_words`).
/// Fast write boundary for the limb rewrite.
pub(crate) fn bi_alloc_int(ctx: &mut dyn NativeContext, v: &crate::bigint::BigInt) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/math/BigInteger", 2);
    // GC-SAFETY (bc math-ec use-after-move, 2026-06-05): `obj` is freshly
    // allocated and NOT yet reachable from any Java root. The `new_array` /
    // `create_string` allocations below can trigger a minor GC that relocates
    // `obj`; the bare local `obj` ObjectRef would then be STALE and the
    // subsequent `set_field(obj, …)` would write through a dangling pointer
    // into whatever object now occupies the old address (the FixedPointTest
    // heap corruption — pinned via the CRATONVM_DBG_ECWATCH watchpoint to the
    // BigInteger-multiply allocation path). Pin `obj` so the moving collector
    // forwards it in place, and re-read the forwarded ref after each allocation.
    let h = ctx.pin_native_root(obj);
    let signum = v.signum();
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let le = v.mag_le(); // little-endian limbs
        let mag_arr = bi_alloc_mag_array(ctx, le.len());
        let obj = ctx.read_native_pin(h, obj);
        // little-endian limbs → big-endian array.
        for (i, &w) in le.iter().rev().enumerate() {
            ctx.set_array_element(mag_arr, i, Value::Int(w as i32));
        }
        ctx.set_field(obj, sig_i, Value::Int(signum));
        ctx.set_field(obj, mag_i, Value::Object(Some(mag_arr)));
        ctx.unpin_native_roots(h);
        obj
    } else {
        let s = ctx.create_string(&v.to_decimal());
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BI_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BI_FIELD_SIGNUM, Value::Int(signum));
        ctx.unpin_native_roots(h);
        obj
    }
}

/// Simple big integer addition using string-based decimal arithmetic.
pub(crate) fn bi_add_str(a: &str, b: &str) -> String {
    let (a_neg, a_abs) = bi_parse_sign(a);
    let (b_neg, b_abs) = bi_parse_sign(b);

    if a_neg == b_neg {
        let sum = bi_add_unsigned(a_abs, b_abs);
        if a_neg {
            format!("-{}", sum)
        } else {
            sum
        }
    } else if a_neg {
        bi_sub_unsigned(b_abs, a_abs)
    } else {
        bi_sub_unsigned(a_abs, b_abs)
    }
}

pub(crate) fn bi_sub_str(a: &str, b: &str) -> String {
    let neg_b = if let Some(stripped) = b.strip_prefix('-') {
        stripped.to_string()
    } else {
        format!("-{}", b)
    };
    bi_add_str(a, &neg_b)
}

pub(crate) fn bi_mul_str(a: &str, b: &str) -> String {
    let (a_neg, a_abs) = bi_parse_sign(a);
    let (b_neg, b_abs) = bi_parse_sign(b);
    let result = bi_mul_unsigned(a_abs, b_abs);
    if result == "0" {
        return "0".to_string();
    }
    if a_neg != b_neg {
        format!("-{}", result)
    } else {
        result
    }
}

pub(crate) fn bi_div_str(a: &str, b: &str) -> String {
    let (a_neg, a_abs) = bi_parse_sign(a);
    let (b_neg, b_abs) = bi_parse_sign(b);
    let result = bi_div_unsigned(a_abs, b_abs);
    if result == "0" {
        return "0".to_string();
    }
    if a_neg != b_neg {
        format!("-{}", result)
    } else {
        result
    }
}

pub(crate) fn bi_mod_str(a: &str, b: &str) -> String {
    // Mirror Java's `BigInteger.remainder` semantics: the sign of the result
    // matches the sign of the dividend (C-style truncated division).
    // `bi_mod_str` is the building block for `bi_mod_inverse_str`, whose
    // extended-Euclidean loop relies on this signed-remainder contract to
    // produce the correct sign of the Bezout coefficient (BC SM2 fix
    // 2026-05-28: BC's `modInverse(p192)` was returning `-x^-1 mod p`
    // because the dropped-sign behaviour collapsed `a_red = (-3) mod 11`
    // from `8` to `3`, off-by-(p-1) in every subsequent step).
    let (a_neg, a_abs) = bi_parse_sign(a);
    let (_b_neg, b_abs) = bi_parse_sign(b);
    let r = bi_mod_unsigned(a_abs, b_abs);
    if a_neg && r != "0" {
        format!("-{r}")
    } else {
        r
    }
}

pub(crate) fn bi_parse_sign(s: &str) -> (bool, &str) {
    if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    }
}

pub(crate) fn bi_add_unsigned(a: &str, b: &str) -> String {
    let a_bytes: Vec<u8> = a.bytes().rev().map(|b| b - b'0').collect();
    let b_bytes: Vec<u8> = b.bytes().rev().map(|b| b - b'0').collect();
    let max_len = a_bytes.len().max(b_bytes.len());
    let mut result = Vec::with_capacity(max_len + 1);
    let mut carry = 0u8;
    for i in 0..max_len {
        let sum =
            a_bytes.get(i).copied().unwrap_or(0) + b_bytes.get(i).copied().unwrap_or(0) + carry;
        result.push(sum % 10);
        carry = sum / 10;
    }
    if carry > 0 {
        result.push(carry);
    }
    let s: String = result.iter().rev().map(|&d| (d + b'0') as char).collect();
    if s.is_empty() {
        "0".to_string()
    } else {
        s
    }
}

pub(crate) fn bi_sub_unsigned(a: &str, b: &str) -> String {
    let cmp = bi_cmp_unsigned(a, b);
    if cmp == 0 {
        return "0".to_string();
    }
    let (larger, smaller, neg) = if cmp > 0 { (a, b, false) } else { (b, a, true) };
    let a_bytes: Vec<u8> = larger.bytes().rev().map(|b| b - b'0').collect();
    let b_bytes: Vec<u8> = smaller.bytes().rev().map(|b| b - b'0').collect();
    let mut result = Vec::with_capacity(a_bytes.len());
    let mut borrow = 0i8;
    for (i, &a_byte) in a_bytes.iter().enumerate() {
        let mut diff = a_byte as i8 - b_bytes.get(i).copied().unwrap_or(0) as i8 - borrow;
        if diff < 0 {
            diff += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        result.push(diff as u8);
    }
    // Remove leading zeros
    while result.len() > 1 && *result.last().unwrap() == 0 {
        result.pop();
    }
    let s: String = result.iter().rev().map(|&d| (d + b'0') as char).collect();
    if neg {
        format!("-{}", s)
    } else {
        s
    }
}

pub(crate) fn bi_mul_unsigned(a: &str, b: &str) -> String {
    let a_bytes: Vec<u8> = a.bytes().rev().map(|b| b - b'0').collect();
    let b_bytes: Vec<u8> = b.bytes().rev().map(|b| b - b'0').collect();
    let mut result = vec![0u8; a_bytes.len() + b_bytes.len()];
    for (i, &ad) in a_bytes.iter().enumerate() {
        let mut carry = 0u16;
        for (j, &bd) in b_bytes.iter().enumerate() {
            let prod = result[i + j] as u16 + ad as u16 * bd as u16 + carry;
            result[i + j] = (prod % 10) as u8;
            carry = prod / 10;
        }
        if carry > 0 {
            result[i + b_bytes.len()] += carry as u8;
        }
    }
    while result.len() > 1 && *result.last().unwrap() == 0 {
        result.pop();
    }
    result.iter().rev().map(|&d| (d + b'0') as char).collect()
}

pub(crate) fn bi_div_unsigned(a: &str, b: &str) -> String {
    if b == "0" {
        return "0".to_string();
    } // Division by zero: return 0 (simplified)
    if bi_cmp_unsigned(a, b) < 0 {
        return "0".to_string();
    }
    // Simple long division
    let mut remainder = String::new();
    let mut quotient = String::new();
    for ch in a.chars() {
        remainder.push(ch);
        // Remove leading zeros from remainder
        while remainder.len() > 1 && remainder.starts_with('0') {
            remainder.remove(0);
        }
        let mut count = 0;
        while bi_cmp_unsigned(&remainder, b) >= 0 {
            remainder = bi_sub_unsigned(&remainder, b);
            count += 1;
        }
        quotient.push((count + b'0') as char);
    }
    while quotient.len() > 1 && quotient.starts_with('0') {
        quotient.remove(0);
    }
    quotient
}

pub(crate) fn bi_mod_unsigned(a: &str, b: &str) -> String {
    if b == "0" {
        return "0".to_string();
    }
    let div = bi_div_unsigned(a, b);
    let prod = bi_mul_unsigned(&div, b);
    bi_sub_unsigned(a, &prod)
}

pub(crate) fn bi_cmp_unsigned(a: &str, b: &str) -> i32 {
    if a.len() != b.len() {
        return if a.len() > b.len() { 1 } else { -1 };
    }
    match a.cmp(b) {
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
    }
}

pub(crate) fn bi_compare(a: &str, b: &str) -> i32 {
    let (a_neg, a_abs) = bi_parse_sign(a);
    let (b_neg, b_abs) = bi_parse_sign(b);
    if a_neg && !b_neg {
        return -1;
    }
    if !a_neg && b_neg {
        return 1;
    }
    let cmp = bi_cmp_unsigned(a_abs, b_abs);
    if a_neg {
        -cmp
    } else {
        cmp
    }
}

/// Convert decimal string to binary string (e.g., "10" -> "1010")
pub(crate) fn bi_to_binary(decimal: &str) -> String {
    if decimal == "0" {
        return "0".to_string();
    }
    let mut val = decimal.to_string();
    let mut bits = Vec::new();
    while val != "0" {
        let rem = bi_mod_unsigned(&val, "2");
        bits.push(if rem == "1" { '1' } else { '0' });
        val = bi_div_unsigned(&val, "2");
    }
    bits.iter().rev().collect()
}

/// Convert binary string back to decimal string
pub(crate) fn bi_from_binary(binary: &str) -> String {
    if binary == "0" || binary.is_empty() {
        return "0".to_string();
    }
    let mut result = "0".to_string();
    for ch in binary.chars() {
        result = bi_mul_unsigned(&result, "2");
        if ch == '1' {
            result = bi_add_unsigned(&result, "1");
        }
    }
    result
}

/// Bitwise AND on two non-negative decimal strings
pub(crate) fn bi_bitwise_and(a: &str, b: &str) -> String {
    let ba = bi_to_binary(a);
    let bb = bi_to_binary(b);
    let max_len = ba.len().max(bb.len());
    let ba_padded: Vec<u8> = format!("{:0>width$}", ba, width = max_len)
        .bytes()
        .collect();
    let bb_padded: Vec<u8> = format!("{:0>width$}", bb, width = max_len)
        .bytes()
        .collect();
    let result: String = ba_padded
        .iter()
        .zip(bb_padded.iter())
        .map(|(&a, &b)| if a == b'1' && b == b'1' { '1' } else { '0' })
        .collect();
    let trimmed = result.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        bi_from_binary(trimmed)
    }
}

/// Bitwise OR on two non-negative decimal strings
pub(crate) fn bi_bitwise_or(a: &str, b: &str) -> String {
    let ba = bi_to_binary(a);
    let bb = bi_to_binary(b);
    let max_len = ba.len().max(bb.len());
    let ba_padded: Vec<u8> = format!("{:0>width$}", ba, width = max_len)
        .bytes()
        .collect();
    let bb_padded: Vec<u8> = format!("{:0>width$}", bb, width = max_len)
        .bytes()
        .collect();
    let result: String = ba_padded
        .iter()
        .zip(bb_padded.iter())
        .map(|(&a, &b)| if a == b'1' || b == b'1' { '1' } else { '0' })
        .collect();
    let trimmed = result.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        bi_from_binary(trimmed)
    }
}

/// Bitwise XOR on two non-negative decimal strings
pub(crate) fn bi_bitwise_xor(a: &str, b: &str) -> String {
    let ba = bi_to_binary(a);
    let bb = bi_to_binary(b);
    let max_len = ba.len().max(bb.len());
    let ba_padded: Vec<u8> = format!("{:0>width$}", ba, width = max_len)
        .bytes()
        .collect();
    let bb_padded: Vec<u8> = format!("{:0>width$}", bb, width = max_len)
        .bytes()
        .collect();
    let result: String = ba_padded
        .iter()
        .zip(bb_padded.iter())
        .map(|(&a, &b)| if a != b { '1' } else { '0' })
        .collect();
    let trimmed = result.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        bi_from_binary(trimmed)
    }
}

// ---------------------------------------------------------------------------
// Arbitrary-precision string helpers for the rest of the BigInteger surface.
// These all preserve full precision (no i128 truncation). They are used by
// the BigInteger natives registered in `phases_late.rs` to replace the buggy
// `p71_bi_val` path which silently truncated to i128 via parse-decimal.
// ---------------------------------------------------------------------------

/// `value << n` for arbitrary-precision signed decimal string `value`.
/// `n` may be negative — that becomes a right shift.
pub(crate) fn bi_shift_left_str(value: &str, n: i32) -> String {
    if value == "0" {
        return "0".to_string();
    }
    if n == 0 {
        return value.to_string();
    }
    if n < 0 {
        return bi_shift_right_str(value, -n);
    }
    // multiply absolute magnitude by 2^n via repeated doubling, preserving sign
    let (neg, abs) = bi_parse_sign(value);
    let mut acc = abs.to_string();
    // Doubling chunked: multiply by 2 n times. For small n (typical shift
    // amounts: 1..512), this is acceptable. For larger n we could mul by
    // a precomputed power-of-two string, but YAGNI.
    for _ in 0..n {
        acc = bi_add_unsigned(&acc, &acc);
    }
    if neg && acc != "0" {
        format!("-{}", acc)
    } else {
        acc
    }
}

/// `value >> n` (arithmetic right shift) for arbitrary-precision signed
/// decimal string `value`. `n` may be negative — that becomes a left shift.
/// Negative inputs use Java's arithmetic-shift semantics: round toward
/// negative infinity (so `-1 >> 1 == -1`, not `0`).
pub(crate) fn bi_shift_right_str(value: &str, n: i32) -> String {
    if value == "0" {
        return "0".to_string();
    }
    if n == 0 {
        return value.to_string();
    }
    if n < 0 {
        return bi_shift_left_str(value, -n);
    }
    let (neg, abs) = bi_parse_sign(value);
    // For positive values: floor-divide by 2^n == divide unsigned by 2^n.
    // For negative values: Java's `x >> n` floors toward -inf, so
    //   `-x >> n == -((x - 1) >> n) - 1` is NOT quite right; the cleaner
    //   identity is: `(-mag) >> n == -ceildiv(mag, 2^n)`.
    let mut q = abs.to_string();
    for _ in 0..n {
        if q == "0" {
            break;
        }
        // q = q / 2 (floor)
        let half = bi_div_unsigned(&q, "2");
        if neg {
            // ceildiv: if q is odd, ceildiv adds 1 after floor-div
            let last_digit = q.bytes().last().map(|b| b - b'0').unwrap_or(0);
            if last_digit & 1 != 0 {
                q = bi_add_unsigned(&half, "1");
            } else {
                q = half;
            }
        } else {
            q = half;
        }
    }
    if neg && q != "0" {
        format!("-{}", q)
    } else if q == "0" && neg {
        // -1 shifted past its bit length still equals -1 in Java arithmetic
        // shift semantics (sign extension fills with 1s). E.g. (-1) >> 100 == -1.
        // We hit q=="0" here because abs is "1" — recover the sign-extended
        // result by returning -1.
        "-1".to_string()
    } else {
        q
    }
}

/// Return bit `n` of the infinite two's-complement representation of
/// `value`. For nonnegative `value`, this is bit `n` of the magnitude.
/// For negative `value`, this is `!(bit n of (mag - 1))`.
pub(crate) fn bi_test_bit_str(value: &str, n: i32) -> bool {
    if n < 0 {
        return false; // BigInteger throws ArithmeticException, but we just return false here.
    }
    if value == "0" {
        return false;
    }
    let (neg, abs) = bi_parse_sign(value);
    // For negative: two's-complement bit n of -mag is !(bit n of (mag - 1))
    let work = if neg {
        bi_sub_unsigned(abs, "1")
    } else {
        abs.to_string()
    };
    // bit n of `work` (which is now an unsigned magnitude string)
    // Compute work >> n, then & 1.
    let mut w = work;
    for _ in 0..n {
        if w == "0" {
            break;
        }
        w = bi_div_unsigned(&w, "2");
    }
    let last = w.bytes().last().map(|b| b - b'0').unwrap_or(0);
    let bit = (last & 1) != 0;
    if neg {
        !bit
    } else {
        bit
    }
}

/// Number of bits in the minimal two's-complement representation of `value`,
/// excluding the sign bit. This matches `java.math.BigInteger.bitLength()`.
pub(crate) fn bi_bit_length_str(value: &str) -> u32 {
    if value == "0" {
        return 0;
    }
    let (neg, abs) = bi_parse_sign(value);
    if neg {
        // For negative values, bitLength = bitLength of magnitude minus
        // 1 if the magnitude is a power of two (since -2^k needs only
        // k bits including sign), else bitLength of magnitude.
        let abs_bits = bi_to_binary(abs).len() as u32;
        // is magnitude a power of two? a power of two has binary "1000...0"
        let bin = bi_to_binary(abs);
        let is_pow2 = bin.starts_with('1') && bin[1..].chars().all(|c| c == '0');
        if is_pow2 {
            abs_bits - 1
        } else {
            abs_bits
        }
    } else {
        bi_to_binary(abs).len() as u32
    }
}

/// `BigInteger.bitCount()`: count of bits that *differ* from the sign bit
/// in the two's-complement representation. For nonnegative values, this is
/// just `popcount(magnitude)`. For negative values, this is the count of
/// zero bits in the lowest `bitLength` bits of `(magnitude - 1)`.
pub(crate) fn bi_bit_count_str(value: &str) -> u32 {
    if value == "0" {
        return 0;
    }
    let (neg, abs) = bi_parse_sign(value);
    if !neg {
        let bin = bi_to_binary(abs);
        bin.bytes().filter(|&b| b == b'1').count() as u32
    } else {
        // Count zero bits in (abs - 1) over its bit width.
        let m1 = bi_sub_unsigned(abs, "1");
        if m1 == "0" {
            // value is -1: in two's complement, -1 is "...111", differs
            // from sign bit (1) in zero positions.
            return 0;
        }
        let bin = bi_to_binary(&m1);
        bin.bytes().filter(|&b| b == b'0').count() as u32
    }
}

/// `BigInteger.not()` — bitwise NOT in two's complement, equivalent to
/// `-(value + 1)`.
pub(crate) fn bi_not_str(value: &str) -> String {
    // -(value + 1)
    let plus1 = bi_add_str(value, "1");
    if plus1 == "0" {
        return "0".to_string();
    }
    if let Some(rest) = plus1.strip_prefix('-') {
        rest.to_string()
    } else {
        format!("-{}", plus1)
    }
}

/// `BigInteger.gcd(other)` — Euclidean GCD on absolute values.
pub(crate) fn bi_gcd_str(a: &str, b: &str) -> String {
    let (_, a_abs) = bi_parse_sign(a);
    let (_, b_abs) = bi_parse_sign(b);
    let mut x = a_abs.to_string();
    let mut y = b_abs.to_string();
    while y != "0" {
        let r = bi_mod_unsigned(&x, &y);
        x = y;
        y = r;
    }
    x
}

/// `BigInteger.modPow(exp, m)` — `base^exp mod m` for arbitrary-precision
/// signed decimal strings. Negative exponents require `modInverse` and are
/// rejected here (Java throws `ArithmeticException`); callers should handle
/// that branch explicitly.
pub(crate) fn bi_mod_pow_str(base: &str, exp: &str, m: &str) -> String {
    if m == "1" || m == "-1" {
        return "0".to_string();
    }
    let (m_neg, m_abs) = bi_parse_sign(m);
    let _ = m_neg; // modulus magnitude is what matters
                   // Reduce base mod m first (Java BigInteger always returns a nonnegative
                   // representative in [0, |m|)).
    let mut b = bi_mod_str(base, m_abs);
    if b.starts_with('-') {
        b = bi_add_str(&b, m_abs);
    }
    // exp must be non-negative for plain modPow.
    let (e_neg, e_abs) = bi_parse_sign(exp);
    if e_neg {
        // Caller is responsible for inverting base first.
        // Fallback: treat as |exp| (consistent with our previous buggy
        // wrapping_mul behavior is not OK; instead return 0 sentinel — but
        // returning 0 is itself a synthetic stub, so panic to be loud).
        panic!("bi_mod_pow_str: negative exponent — caller must compute modInverse first");
    }
    let mut result = "1".to_string();
    // Iterate bits of exp from LSB to MSB by repeated div2.
    let mut e = e_abs.to_string();
    while e != "0" {
        let last = e.bytes().last().map(|b| b - b'0').unwrap_or(0);
        if last & 1 == 1 {
            result = bi_mod_unsigned(&bi_mul_unsigned(&result, &b), m_abs);
        }
        e = bi_div_unsigned(&e, "2");
        if e != "0" {
            b = bi_mod_unsigned(&bi_mul_unsigned(&b, &b), m_abs);
        }
    }
    result
}

/// `BigInteger.modInverse(m)` — extended Euclidean on arbitrary-precision
/// signed decimal strings. Returns `None` if `gcd(a, m) != 1`.
pub(crate) fn bi_mod_inverse_str(a: &str, m: &str) -> Option<String> {
    let (_, m_abs) = bi_parse_sign(m);
    // Reduce a mod m_abs first (positive representative).
    let a_red = {
        let r = bi_mod_str(a, m_abs);
        if r.starts_with('-') {
            bi_add_str(&r, m_abs)
        } else {
            r
        }
    };
    // Extended GCD on (a_red, m_abs).
    let mut old_r = a_red;
    let mut r = m_abs.to_string();
    let mut old_s = "1".to_string();
    let mut s = "0".to_string();
    while r != "0" {
        let q = bi_div_str(&old_r, &r);
        let new_r = bi_sub_str(&old_r, &bi_mul_str(&q, &r));
        old_r = std::mem::replace(&mut r, new_r);
        let new_s = bi_sub_str(&old_s, &bi_mul_str(&q, &s));
        old_s = std::mem::replace(&mut s, new_s);
    }
    if old_r != "1" {
        return None;
    }
    // Result = old_s mod m_abs, in [0, m_abs)
    let mut inv = bi_mod_str(&old_s, m_abs);
    if inv.starts_with('-') {
        inv = bi_add_str(&inv, m_abs);
    }
    Some(inv)
}

#[cfg(test)]
mod biginteger_modpow_modinverse_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::{bi_alloc_int, bi_mod_inverse_str, bi_mod_pow_str};
    use crate::bigint::BigInt;
    use crate::test_utils::mock_ctx;

    // --- modPow sign handling (the registered native delegates to these
    //     helpers; these tests pin the underlying arithmetic that the old
    //     sign-stripping native got wrong) ---

    #[test]
    fn modpow_positive() {
        // 3^4 mod 7 = 81 mod 7 = 4
        assert_eq!(bi_mod_pow_str("3", "4", "7"), "4");
    }

    #[test]
    fn modpow_negative_base_reduced_mod_m() {
        // (-3)^2 mod 7: -3 ≡ 4 (mod 7); 4^2 = 16 ≡ 2 (mod 7).
        // The old native stripped the sign and computed 3^2 = 9 ≡ 2 — same
        // residue for an even exponent, so use an ODD exponent to expose it:
        // (-3)^3 mod 7: 4^3 = 64 ≡ 1 (mod 7); sign-stripped 3^3 = 27 ≡ 6.
        assert_eq!(bi_mod_pow_str("-3", "3", "7"), "1");
        assert_ne!(bi_mod_pow_str("-3", "3", "7"), "6");
    }

    #[test]
    fn modpow_large_operands() {
        // 2^256 mod 1000000007. The intermediate 2^256 is far beyond i128, so
        // this exercises the arbitrary-precision string path end to end.
        // Cross-checked: Python pow(2, 256, 1000000007) == 792845266.
        assert_eq!(bi_mod_pow_str("2", "256", "1000000007"), "792845266");

        // Same exponent with a modulus that is itself larger than i128
        // (Mersenne prime 2^61 - 1): since 2^61 ≡ 1, 2^256 ≡ 2^(256 mod 61) =
        // 2^12 = 4096. Cross-checked: pow(2, 256, 2305843009213693951) == 4096.
        assert_eq!(bi_mod_pow_str("2", "256", "2305843009213693951"), "4096");
    }

    // --- modInverse: real arbitrary-precision inverse + non-invertible None ---

    #[test]
    fn modinverse_small() {
        // 3 * 5 = 15 ≡ 1 (mod 7), so 3^-1 ≡ 5 (mod 7).
        assert_eq!(bi_mod_inverse_str("3", "7"), Some("5".to_string()));
    }

    #[test]
    fn modinverse_negative_base() {
        // -3 ≡ 4 (mod 7); 4^-1 ≡ 2 (mod 7) since 4*2 = 8 ≡ 1.
        assert_eq!(bi_mod_inverse_str("-3", "7"), Some("2".to_string()));
    }

    #[test]
    fn modinverse_non_coprime_is_none() {
        // gcd(4, 8) = 4 != 1 -> not invertible (native maps None -> ArithmeticException).
        assert_eq!(bi_mod_inverse_str("4", "8"), None);
        // gcd(6, 9) = 3 != 1.
        assert_eq!(bi_mod_inverse_str("6", "9"), None);
    }

    #[test]
    fn modinverse_large_operands_not_silent_one() {
        // A large prime modulus far beyond i128; the old i128 fallback returned
        // a silent "1" for inputs this size. Verify a real inverse: with
        // m = 2^61 - 1 (Mersenne prime 2305843009213693951) and a = 2,
        // 2^-1 mod m = (m+1)/2 = 1152921504606846976.
        assert_eq!(
            bi_mod_inverse_str("2", "2305843009213693951"),
            Some("1152921504606846976".to_string())
        );
    }

    #[test]
    fn bi_alloc_int_releases_native_pin() {
        let mut ctx = mock_ctx();
        let value = BigInt::from_decimal("123456789012345678901234567890");

        assert_eq!(ctx.native_pin_count_for_test(), 0);
        let _ = bi_alloc_int(&mut ctx, &value);
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }
}

/// `BigInteger.toByteArray()` — two's-complement big-endian byte encoding,
/// with the minimal length needed to represent the value (always at least
/// one byte). Sign-extends.
pub(crate) fn bi_to_byte_array_str(value: &str) -> Vec<u8> {
    if value == "0" {
        return vec![0u8];
    }
    let (neg, abs) = bi_parse_sign(value);
    // First build the magnitude as big-endian bytes.
    let mut mag_bytes: Vec<u8> = Vec::new();
    let mut q = abs.to_string();
    while q != "0" {
        // q mod 256, q = q / 256
        // Compute mod 256 via decimal long division by 256.
        let mut rem: u32 = 0;
        let mut next_q = String::new();
        for ch in q.chars() {
            let d = ch.to_digit(10).unwrap_or(0);
            let cur = rem * 10 + d;
            let qd = cur / 256;
            rem = cur % 256;
            if !(next_q.is_empty() && qd == 0) {
                next_q.push(char::from_digit(qd, 10).unwrap());
            }
        }
        if next_q.is_empty() {
            next_q.push('0');
        }
        mag_bytes.push(rem as u8);
        q = next_q;
    }
    // mag_bytes is currently little-endian (LSB first). Reverse for BE.
    mag_bytes.reverse();
    if !neg {
        // Positive: prepend 0x00 if high bit is set, so sign bit reads as +.
        if mag_bytes[0] & 0x80 != 0 {
            let mut out = Vec::with_capacity(mag_bytes.len() + 1);
            out.push(0);
            out.extend_from_slice(&mag_bytes);
            out
        } else {
            mag_bytes
        }
    } else {
        // Negative: two's complement = invert all bits of (mag - 1)... actually
        // simpler: (~mag + 1) bytewise, but we have mag, want -mag.
        // Compute (2^(8*len) - mag) interpreted as len bytes; if that result's
        // high bit is 0, prepend 0xFF so sign reads as negative.
        // Easy path: do bytewise (256 - byte) with borrow.
        let len = mag_bytes.len();
        let mut twos = vec![0u8; len];
        let mut borrow: u16 = 0;
        // Process from LSB (end of mag_bytes) to MSB (start).
        for i in (0..len).rev() {
            let m = mag_bytes[i] as i32;
            let mut diff = 0i32 - m - borrow as i32;
            if diff < 0 {
                diff += 256;
                borrow = 1;
            } else {
                borrow = 0;
            }
            twos[i] = diff as u8;
        }
        // If high bit of twos[0] is 0, we need to prepend 0xFF to keep sign.
        if twos[0] & 0x80 == 0 {
            let mut out = Vec::with_capacity(len + 1);
            out.push(0xFF);
            out.extend_from_slice(&twos);
            out
        } else {
            // Trim leading 0xFF bytes as long as the next byte still has
            // high bit set (sign extension).
            let mut start = 0usize;
            while start + 1 < twos.len() && twos[start] == 0xFF && (twos[start + 1] & 0x80) != 0 {
                start += 1;
            }
            twos[start..].to_vec()
        }
    }
}

/// `BigInteger(byte[])` — interpret `bytes` as a signed two's-complement
/// big-endian integer and return the decimal string.
pub(crate) fn bi_from_byte_array_signed(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0".to_string();
    }
    let negative = bytes[0] & 0x80 != 0;
    // For positive: just build the magnitude from the bytes (treat as unsigned).
    // For negative: build the magnitude as ~bytes + 1 (two's complement).
    let mag_bytes: Vec<u8> = if !negative {
        bytes.to_vec()
    } else {
        // Invert + add 1 with borrow propagation.
        let mut inv: Vec<u8> = bytes.iter().map(|b| !b).collect();
        let mut carry: u16 = 1;
        for byte in inv.iter_mut().rev() {
            let v = *byte as u16 + carry;
            *byte = (v & 0xFF) as u8;
            carry = v >> 8;
            if carry == 0 {
                break;
            }
        }
        inv
    };
    // Convert big-endian bytes to decimal string (multiply by 256 each step).
    let mut decimal = "0".to_string();
    for &b in &mag_bytes {
        decimal = bi_mul_unsigned(&decimal, "256");
        if b != 0 {
            decimal = bi_add_unsigned(&decimal, &b.to_string());
        }
    }
    if decimal == "0" {
        "0".to_string()
    } else if negative {
        format!("-{}", decimal)
    } else {
        decimal
    }
}

/// `BigInteger(int signum, byte[] magnitude)` — interpret `bytes` as an
/// unsigned big-endian magnitude and apply the given `signum`.
pub(crate) fn bi_from_byte_array_with_signum(signum: i32, bytes: &[u8]) -> String {
    let mut decimal = "0".to_string();
    for &b in bytes {
        decimal = bi_mul_unsigned(&decimal, "256");
        if b != 0 {
            decimal = bi_add_unsigned(&decimal, &b.to_string());
        }
    }
    if decimal == "0" {
        "0".to_string()
    } else if signum < 0 {
        format!("-{}", decimal)
    } else {
        decimal
    }
}

/// `BigInteger.isProbablePrime(certainty)` — trial division by small odd
/// primes (3, 5, 7, …, 999). For the certainty levels used by callers like
/// BouncyCastle's EC parameter setup, a value that survives trial division
/// to the small-prime threshold and is itself ≥ 2 is reported as probably
/// prime. (This is a pragmatic stand-in for full Miller–Rabin, sufficient
/// for the BC EC parameter validation use-case where the curve primes
/// genuinely are prime.)
pub(crate) fn bi_is_probable_prime_str(value: &str) -> bool {
    let (neg, abs) = bi_parse_sign(value);
    if neg || abs == "0" || abs == "1" {
        return false;
    }
    if abs == "2" || abs == "3" {
        return true;
    }
    // Even?
    let last = abs.bytes().last().map(|b| b - b'0').unwrap_or(0);
    if last & 1 == 0 {
        return false;
    }
    let mut i: u32 = 3;
    while i < 1000 {
        let divisor = i.to_string();
        // Skip if divisor exceeds value (only possible for very small `abs`,
        // which we already handled above).
        if bi_cmp_unsigned(&divisor, abs) > 0 {
            break;
        }
        if bi_mod_unsigned(abs, &divisor) == "0" {
            // value == divisor itself is OK (i.e. the divisor IS the value);
            // any larger multiple means composite.
            if bi_cmp_unsigned(&divisor, abs) == 0 {
                return true;
            }
            return false;
        }
        i += 2;
    }
    // No small factor < 1000. The old code returned `true` here — a
    // trial-division-only test that wrongly classifies EVERY large
    // composite with no small factor as prime (e.g. a product of two
    // 256-bit primes, the RSA/Miller-Rabin stress case). That broke
    // `BigInteger.isProbablePrime` and, transitively,
    // `BigIntegers.createRandomPrime` (which validates candidates via
    // `isProbablePrime`, so it returned composites), failing BouncyCastle
    // `PrimesTest`. Decide it properly with a real Miller-Rabin test.
    bi_miller_rabin_str(abs)
}

/// Deterministic-base Miller-Rabin probable-prime test on the unsigned
/// decimal magnitude `n`. `n` is assumed odd and to have no prime factor
/// below 1000 (the caller filters those first). Uses a fixed set of small
/// prime bases: this is the standard "strong probable prime" test —
/// deterministic for n below ~3.3e24 (first 13 bases) and astronomically
/// reliable beyond that, matching `BigInteger.isProbablePrime`'s contract.
pub(crate) fn bi_miller_rabin_str(n: &str) -> bool {
    // Write n-1 = d * 2^s with d odd.
    let n_minus_1 = bi_sub_str(n, "1");
    let mut d = n_minus_1.clone();
    let mut s: u32 = 0;
    loop {
        let last = d.bytes().last().map(|b| b - b'0').unwrap_or(0);
        if last & 1 == 1 {
            break;
        }
        d = bi_div_str(&d, "2");
        s += 1;
    }
    const BASES: &[u32] = &[2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41];
    for &a in BASES {
        let a_str = a.to_string();
        // base must be in [2, n-2]; for the large n we reach here this is
        // always true, but guard for safety.
        if bi_cmp_unsigned(&a_str, n) >= 0 {
            continue;
        }
        // x = a^d mod n
        let mut x = bi_mod_pow_str(&a_str, &d, n);
        if bi_cmp_unsigned(&x, "1") == 0 || bi_cmp_unsigned(&x, &n_minus_1) == 0 {
            continue; // probable prime for this base
        }
        let mut witnessed_composite = true;
        for _ in 0..s.saturating_sub(1) {
            // x = x^2 mod n
            x = bi_mod_unsigned(&bi_mul_unsigned(&x, &x), n);
            if bi_cmp_unsigned(&x, &n_minus_1) == 0 {
                witnessed_composite = false;
                break;
            }
        }
        if witnessed_composite {
            return false; // definitely composite
        }
    }
    true
}

/// RBIGDEC.1 — register BigInteger arithmetic + toString overrides for
/// real-JDK mode. The synthetic-jdk-only `register_biginteger_natives`
/// registers the full surface; this lean variant covers the methods the
/// `BdProbe` tests + KC16 boot path actually call, so we don't perturb
/// real-JDK behaviour for the broader class.
pub(crate) fn register_biginteger_arithmetic_overrides(registry: &mut NativeMethodRegistry) {
    // census-tag: BigInteger arithmetic is spec-exact (@IntrinsicCandidate
    // territory) and must match the real JDK bytecode → Intrinsic.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let bi = "java/math/BigInteger";
    registry.register(
        bi,
        "add",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_add,
    );
    registry.register(
        bi,
        "subtract",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_subtract,
    );
    registry.register(
        bi,
        "multiply",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_multiply,
    );
    registry.register(bi, "negate", "()Ljava/math/BigInteger;", native_bi_negate);
    registry.register(bi, "signum", "()I", native_bi_signum);
    registry.register(bi, "toString", "()Ljava/lang/String;", native_bi_to_string);
    registry.register(bi, "intValue", "()I", native_bi_int_value);
    registry.register(bi, "longValue", "()J", native_bi_long_value);
    // `pow` — the real bytecode delegates to interpreted square/multiply
    // loops over int[] (squareToLen has no shadowable call boundary).
    // `new BigDecimal(double)` computes 5^(-exponent) through here (52+
    // squarings per ctor for a random double), which made Lucene
    // TestUtil.nextLong's large-range branch time out ES codec tests.
    // The native is spec-exact limb square-and-multiply.
    registry.register(bi, "pow", "(I)Ljava/math/BigInteger;", native_bi_pow);
    // `valueOf(long)` — constructors are JIT-banned (skip_list A1.4), so the
    // real bytecode's `new BigInteger(long)` runs interpreted on every call;
    // the valueOf→<init>(J)→Number.<init> frame chain was ~26% of watchdog
    // samples in the ES doc-values timeout. Fresh instances are
    // spec-compliant (the JDK's -16..16 cache is an optional optimization).
    registry.register(
        bi,
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        native_bi_value_of,
    );
    // `compareTo` — the JDK 25 bytecode walks `mag:[I` word-by-word and uses
    // `Integer.compareUnsigned`. Our `mag:[I` is populated by `bi_alloc` for
    // values constructed via Rust natives, but BigIntegers that originate from
    // JDK bytecode (constants, `valueOf`, hex-string ctor) may take a different
    // initialization path whose mag-layout disagrees with the unsigned-compare
    // walk — BC's `ECCurve.Fp.fromBigInteger` then sees `compareTo(q) == 0`
    // for every value near q and throws "x value invalid for Fp field element"
    // on every X9 curve point. Route through `bi_compare` (signed decimal
    // compare via `bi_cmp_unsigned`) which works regardless of mag layout.
    registry.register(
        bi,
        "compareTo",
        "(Ljava/math/BigInteger;)I",
        native_bi_compare_to,
    );
    // Erased Comparable<BigInteger>.compareTo(Object) bridge.
    registry.register(
        bi,
        "compareTo",
        "(Ljava/lang/Object;)I",
        native_bi_compare_to,
    );
    registry.register(bi, "equals", "(Ljava/lang/Object;)Z", native_bi_equals);
    registry.set_category(__prev_cat);
}

// `native_bi_compare_to` and `native_bi_equals` are defined further down in
// this file (~line 24386 / 24400) inside the synthetic-mode BigInteger
// register block. They handle the real-JDK `signum`+`mag` layout transparently
// via `bi_read`, so the same implementations work in both modes.

/// RBIGDEC.1 — register BigDecimal arithmetic + toString overrides for
/// real-JDK mode.  Same rationale as `register_biginteger_arithmetic_overrides`.
pub(crate) fn register_bigdecimal_arithmetic_overrides(registry: &mut NativeMethodRegistry) {
    // census-tag: BigDecimal arithmetic is spec-exact and must match real JDK
    // bytecode → Intrinsic.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let bd = "java/math/BigDecimal";
    registry.register(
        bd,
        "add",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_add,
    );
    registry.register(
        bd,
        "subtract",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_subtract,
    );
    registry.register(
        bd,
        "multiply",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_multiply,
    );
    registry.register(bd, "negate", "()Ljava/math/BigDecimal;", native_bd_negate);
    registry.register(bd, "signum", "()I", native_bd_signum);
    registry.register(bd, "scale", "()I", native_bd_scale);
    registry.register(bd, "precision", "()I", native_bd_precision);
    registry.register(
        bd,
        "valueOf",
        "(J)Ljava/math/BigDecimal;",
        native_bd_value_of_long,
    );
    registry.register(
        bd,
        "valueOf",
        "(D)Ljava/math/BigDecimal;",
        native_bd_value_of_double,
    );
    registry.register(bd, "toString", "()Ljava/lang/String;", native_bd_to_string);
    registry.register(
        bd,
        "toPlainString",
        "()Ljava/lang/String;",
        native_bd_to_plain_string,
    );
    registry.register(bd, "intValue", "()I", native_bd_int_value);
    registry.register(bd, "longValue", "()J", native_bd_long_value);
    registry.register(bd, "doubleValue", "()D", native_bd_double_value);
    // `setScale`/`toBigInteger` — the real bytecode routes through
    // `divideAndRound` → MutableBigInteger long division, all interpreted;
    // these natives are exact limb divmod + RoundingMode semantics
    // (verified against HotSpot across every mode). `setScale(int,
    // RoundingMode)` needs no entry: its bytecode reads `oldMode` and
    // delegates to `(II)`, which lands here.
    registry.register(
        bd,
        "setScale",
        "(I)Ljava/math/BigDecimal;",
        native_bd_set_scale,
    );
    registry.register(
        bd,
        "setScale",
        "(II)Ljava/math/BigDecimal;",
        native_bd_set_scale_rounding,
    );
    registry.register(
        bd,
        "toBigInteger",
        "()Ljava/math/BigInteger;",
        native_bd_to_big_integer,
    );
    // Hot constructors — `<init>` is JIT-banned (skip_list A1.4), so the
    // real ctor bytecode runs interpreted on every allocation. Both natives
    // are exact: `(D)` produces the double's exact binary expansion
    // (sign/exponent/significand decomposition, matching the real ctor
    // digit-for-digit — verified vs HotSpot incl. 0.1's 55-digit form),
    // `(BigInteger)` mirrors the compactValFor split. `<init>` natives are
    // dispatched in real-JDK mode (java/util/Random's seeded ctor already
    // relies on this).
    registry.register(bd, "<init>", "(D)V", native_bd_init_double);
    registry.register(
        bd,
        "<init>",
        "(Ljava/math/BigInteger;)V",
        native_bd_init_bigint,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_biginteger_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: BigInteger spec-exact arithmetic/factories → Intrinsic.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let bi = "java/math/BigInteger";

    registry.register(bi, "<init>", "(Ljava/lang/String;)V", native_bi_init_string);
    registry.register(
        bi,
        "<init>",
        "(Ljava/lang/String;I)V",
        native_bi_init_string_radix,
    );
    registry.register(
        bi,
        "valueOf",
        "(J)Ljava/math/BigInteger;",
        native_bi_value_of,
    );
    registry.register(
        bi,
        "add",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_add,
    );
    registry.register(
        bi,
        "subtract",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_subtract,
    );
    registry.register(
        bi,
        "multiply",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_multiply,
    );
    registry.register(
        bi,
        "divide",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_divide,
    );
    registry.register(
        bi,
        "mod",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_mod,
    );
    registry.register(
        bi,
        "remainder",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_mod,
    );
    registry.register(bi, "negate", "()Ljava/math/BigInteger;", native_bi_negate);
    registry.register(bi, "abs", "()Ljava/math/BigInteger;", native_bi_abs);
    registry.register(
        bi,
        "compareTo",
        "(Ljava/math/BigInteger;)I",
        native_bi_compare_to,
    );
    registry.register(bi, "equals", "(Ljava/lang/Object;)Z", native_bi_equals);
    registry.register(bi, "toString", "()Ljava/lang/String;", native_bi_to_string);
    registry.register(
        bi,
        "toString",
        "(I)Ljava/lang/String;",
        native_bi_to_string_radix,
    );
    registry.register(bi, "intValue", "()I", native_bi_int_value);
    registry.register(bi, "longValue", "()J", native_bi_long_value);
    registry.register(bi, "doubleValue", "()D", native_bi_double_value);
    registry.register(bi, "floatValue", "()F", native_bi_float_value);
    registry.register(bi, "signum", "()I", native_bi_signum);
    registry.register(bi, "hashCode", "()I", native_bi_hash_code);
    registry.register(bi, "pow", "(I)Ljava/math/BigInteger;", native_bi_pow);
    registry.register(
        bi,
        "max",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_max,
    );
    registry.register(
        bi,
        "min",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        native_bi_min,
    );

    // Constants
    registry.register(bi, "ZERO", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")))))
    });
    registry.register(bi, "ONE", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "1")))))
    });
    registry.register(bi, "TEN", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "10")))))
    });
    registry.register(bi, "TWO", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "2")))))
    });

    // --- BigInteger additional methods (Phase 47) ---

    // gcd — greatest common divisor (Euclidean algorithm using string-based mod)
    registry.register(
        bi,
        "gcd",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let mut a = bi_read(ctx, this).trim_start_matches('-').to_string();
            let mut b = bi_read(ctx, other).trim_start_matches('-').to_string();
            if a == "0" {
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, &b)))));
            }
            if b == "0" {
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, &a)))));
            }
            while b != "0" {
                let t = b.clone();
                b = bi_mod_unsigned(&a, &t);
                a = t;
            }
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &a)))))
        },
    );

    // bitLength — number of bits in the magnitude (string-based)
    registry.register(bi, "bitLength", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = bi_read(ctx, this).trim_start_matches('-').to_string();
        if s == "0" {
            return Ok(Some(Value::Int(0)));
        }
        let mut val = s;
        let mut bits = 0;
        while val != "0" {
            val = bi_div_unsigned(&val, "2");
            bits += 1;
        }
        Ok(Some(Value::Int(bits)))
    });

    // bitCount — number of set bits (string-based)
    registry.register(bi, "bitCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = bi_read(ctx, this).trim_start_matches('-').to_string();
        if s == "0" {
            return Ok(Some(Value::Int(0)));
        }
        let mut val = s;
        let mut count = 0;
        while val != "0" {
            let rem = bi_mod_unsigned(&val, "2");
            if rem == "1" {
                count += 1;
            }
            val = bi_div_unsigned(&val, "2");
        }
        Ok(Some(Value::Int(count)))
    });

    // testBit — test bit at index (string-based)
    registry.register(bi, "testBit", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bit = match args.get(1) {
            Some(Value::Int(b)) => *b,
            _ => 0,
        };
        let s = bi_read(ctx, this).trim_start_matches('-').to_string();
        if s == "0" {
            return Ok(Some(Value::Int(0)));
        }
        let mut val = s;
        for _ in 0..bit {
            val = bi_div_unsigned(&val, "2");
            if val == "0" {
                return Ok(Some(Value::Int(0)));
            }
        }
        let rem = bi_mod_unsigned(&val, "2");
        Ok(Some(Value::Int(if rem == "1" { 1 } else { 0 })))
    });

    // shiftLeft — multiply by 2^n (string-based)
    registry.register(bi, "shiftLeft", "(I)Ljava/math/BigInteger;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let s = bi_read(ctx, this);
        let (neg, abs) = bi_parse_sign(&s);
        let mut result = abs.to_string();
        if n > 0 {
            for _ in 0..n {
                result = bi_mul_unsigned(&result, "2");
            }
        } else if n < 0 {
            for _ in 0..(-n) {
                result = bi_div_unsigned(&result, "2");
            }
        }
        let final_str = if neg && result != "0" {
            format!("-{}", result)
        } else {
            result
        };
        Ok(Some(Value::Object(Some(bi_alloc(ctx, &final_str)))))
    });

    // shiftRight — divide by 2^n (string-based)
    registry.register(
        bi,
        "shiftRight",
        "(I)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let n = match args.get(1) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let s = bi_read(ctx, this);
            let (neg, abs) = bi_parse_sign(&s);
            let mut result = abs.to_string();
            if n > 0 {
                for _ in 0..n {
                    result = bi_div_unsigned(&result, "2");
                }
            } else if n < 0 {
                for _ in 0..(-n) {
                    result = bi_mul_unsigned(&result, "2");
                }
            }
            let final_str = if neg && result != "0" {
                format!("-{}", result)
            } else {
                result
            };
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &final_str)))))
        },
    );

    // Bitwise operations: and, or, xor, not (string-based)
    registry.register(
        bi,
        "and",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let a = bi_read(ctx, this).trim_start_matches('-').to_string();
            let b = bi_read(ctx, other).trim_start_matches('-').to_string();
            let result = bi_bitwise_and(&a, &b);
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &result)))))
        },
    );
    registry.register(
        bi,
        "or",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let a = bi_read(ctx, this).trim_start_matches('-').to_string();
            let b = bi_read(ctx, other).trim_start_matches('-').to_string();
            let result = bi_bitwise_or(&a, &b);
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &result)))))
        },
    );
    registry.register(
        bi,
        "xor",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let a = bi_read(ctx, this).trim_start_matches('-').to_string();
            let b = bi_read(ctx, other).trim_start_matches('-').to_string();
            let result = bi_bitwise_xor(&a, &b);
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &result)))))
        },
    );
    registry.register(bi, "not", "()Ljava/math/BigInteger;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = bi_read(ctx, this);
        // Java BigInteger.not() returns -(this + 1) per the spec
        let neg_result = bi_add_str(&s, "1");
        let final_str = if neg_result.starts_with('-') {
            neg_result[1..].to_string()
        } else if neg_result == "0" {
            "-1".to_string()
        } else {
            format!("-{}", neg_result)
        };
        Ok(Some(Value::Object(Some(bi_alloc(ctx, &final_str)))))
    });

    // toByteArray — convert to two's complement byte array
    registry.register(bi, "toByteArray", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = bi_read(ctx, this);
        // Try i128 first (covers most cases)
        if let Ok(val) = s.parse::<i128>() {
            let bytes = val.to_be_bytes();
            let start = if val >= 0 {
                bytes.iter().position(|&b| b != 0).unwrap_or(15).min(15)
            } else {
                bytes
                    .iter()
                    .position(|&b| b != 0xFF)
                    .unwrap_or(15)
                    .saturating_sub(1)
            };
            let significant = &bytes[start..];
            let arr = ctx.new_array(
                cratonvm_types::ArrayElementType::Byte,
                significant.len().max(1),
            );
            for (i, &b) in significant.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
        // For numbers beyond i128 range, use binary conversion
        let (_neg, abs) = bi_parse_sign(&s);
        let binary = bi_to_binary(abs);
        let pad_len = (8 - (binary.len() % 8)) % 8;
        let padded = format!("{}{}", "0".repeat(pad_len + 8), binary); // extra byte for sign
        let byte_count = padded.len() / 8;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, byte_count);
        for i in 0..byte_count {
            let byte_str = &padded[i * 8..(i + 1) * 8];
            let byte_val = u8::from_str_radix(byte_str, 2).unwrap_or(0);
            ctx.set_array_element(arr, i, Value::Int(byte_val as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    });

    // valueOf(long) — create from long value
    registry.register(bi, "valueOf", "(J)Ljava/math/BigInteger;", |ctx, args| {
        let val = match args.first() {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Object(Some(bi_alloc(ctx, &val.to_string())))))
    });

    // isProbablePrime — string-based trial division
    registry.register(bi, "isProbablePrime", "(I)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = bi_read(ctx, this).trim_start_matches('-').to_string();
        if s == "0" || s == "1" {
            return Ok(Some(Value::Int(0)));
        }
        if s == "2" || s == "3" {
            return Ok(Some(Value::Int(1)));
        }
        let last_digit: u8 = s.bytes().last().unwrap_or(b'0') - b'0';
        if last_digit % 2 == 0 {
            return Ok(Some(Value::Int(0)));
        }
        if bi_mod_unsigned(&s, "3") == "0" {
            return Ok(Some(Value::Int(0)));
        }
        let mut i = 5u64;
        while i <= 10000 {
            let is = i.to_string();
            let isq = bi_mul_unsigned(&is, &is);
            if bi_cmp_unsigned(&isq, &s) > 0 {
                break;
            }
            if bi_mod_unsigned(&s, &is) == "0" {
                return Ok(Some(Value::Int(0)));
            }
            let i2 = (i + 2).to_string();
            if bi_mod_unsigned(&s, &i2) == "0" {
                return Ok(Some(Value::Int(0)));
            }
            i += 6;
        }
        Ok(Some(Value::Int(1)))
    });

    // modPow(exponent, modulus) -> BigInteger
    //
    // Matches `java.math.BigInteger.modPow` semantics exactly (do NOT strip
    // operand signs — the previous implementation `trim_start_matches('-')` on
    // base AND exponent silently produced wrong results):
    //   * modulus.signum() <= 0  -> ArithmeticException("BigInteger: modulus not positive")
    //   * the base is reduced into the canonical nonnegative residue [0, m)
    //     BEFORE exponentiation (a negative base is congruent to base + m, not
    //     to |base|), so e.g. (-3)^2 mod 7 == 2, not 9 mod 7.
    //   * a negative exponent is legal iff the base is invertible mod m: the
    //     result is modInverse(base, m)^|exp| mod m. If gcd(base, m) != 1 we
    //     throw ArithmeticException("BigInteger not invertible.") — matching the
    //     exception the JDK propagates from the internal modInverse.
    registry.register(
        bi,
        "modPow",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let exp_obj = obj_arg(args, 1)?;
            let mod_obj = obj_arg(args, 2)?;
            // Signed decimal strings (bi_read preserves the leading '-').
            let base = bi_read(ctx, this);
            let exp = bi_read(ctx, exp_obj);
            let modulus = bi_read(ctx, mod_obj);
            // JDK: modulus must be strictly positive.
            if modulus.starts_with('-') || modulus == "0" {
                return Err(RuntimeError::ArithmeticException {
                    message: "BigInteger: modulus not positive".to_string(),
                }
                .into());
            }
            if modulus == "1" {
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")))));
            }
            // For a negative exponent, invert the (sign-reduced) base first and
            // raise the inverse to |exp|. bi_mod_inverse_str returns None when
            // gcd(base, m) != 1 (base not invertible).
            let result = if exp.starts_with('-') {
                let inv = match bi_mod_inverse_str(&base, &modulus) {
                    Some(inv) => inv,
                    None => {
                        return Err(RuntimeError::ArithmeticException {
                            message: "BigInteger not invertible.".to_string(),
                        }
                        .into());
                    }
                };
                let abs_exp = exp.trim_start_matches('-');
                bi_mod_pow_str(&inv, abs_exp, &modulus)
            } else {
                // Non-negative exponent: bi_mod_pow_str reduces the (possibly
                // negative) base into [0, m) internally.
                bi_mod_pow_str(&base, &exp, &modulus)
            };
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &result)))))
        },
    );

    // modInverse(modulus) -> BigInteger (extended Euclidean algorithm)
    //
    // Uses the arbitrary-precision string extended-Euclidean helper
    // (bi_mod_inverse_str) for ALL operand sizes. The previous implementation
    // only handled values that fit in i128 and silently returned 1 for anything
    // larger (e.g. every RSA-sized modulus) AND never threw on non-coprime
    // input — both dangerous if a crypto path reaches it. Per the JDK contract:
    //   * modulus.signum() <= 0  -> ArithmeticException("BigInteger: modulus not positive")
    //   * gcd(this, m) != 1      -> ArithmeticException("BigInteger not invertible.")
    // bi_mod_inverse_str reduces a (possibly negative) base mod m internally and
    // returns the canonical inverse in [0, m); it yields None exactly when the
    // input is not invertible.
    registry.register(
        bi,
        "modInverse",
        "(Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mod_obj = obj_arg(args, 1)?;
            let a_str = bi_read(ctx, this);
            let m_str = bi_read(ctx, mod_obj);
            if m_str.starts_with('-') || m_str == "0" {
                return Err(RuntimeError::ArithmeticException {
                    message: "BigInteger: modulus not positive".to_string(),
                }
                .into());
            }
            // Modulus 1: every value is congruent to 0, and 0 is its own (only)
            // residue; the JDK returns 0 here (a^-1 mod 1 == 0).
            if m_str == "1" {
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")))));
            }
            match bi_mod_inverse_str(&a_str, &m_str) {
                Some(inv) => Ok(Some(Value::Object(Some(bi_alloc(ctx, &inv))))),
                None => Err(RuntimeError::ArithmeticException {
                    message: "BigInteger not invertible.".to_string(),
                }
                .into()),
            }
        },
    );
    registry.set_category(__prev_cat);
}

fn native_bi_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => "0".to_string(),
    };
    bi_write_into(ctx, this, &s);
    Ok(None)
}

/// Populate an existing `BigInteger` instance with the value parsed from a
/// decimal string.  Picks the slot layout (real-JDK signum/mag vs. legacy
/// synthetic value/signum) automatically.  Used by the `<init>` natives so
/// `new BigInteger("17")` lands in the right slots regardless of JDK mode.
fn bi_write_into(ctx: &mut dyn NativeContext, this: ObjectRef, value: &str) {
    let signum = if value.starts_with('-') {
        -1
    } else if value == "0" {
        0
    } else {
        1
    };
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let mag_words = decimal_to_mag_words(value);
        let mag_arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, mag_words.len());
        for (i, w) in mag_words.iter().enumerate() {
            ctx.set_array_element(mag_arr, i, Value::Int(*w as i32));
        }
        ctx.set_field(this, sig_i, Value::Int(signum));
        ctx.set_field(this, mag_i, Value::Object(Some(mag_arr)));
    } else {
        let val_str = ctx.create_string(value);
        ctx.set_field(this, BI_FIELD_VALUE, Value::Object(Some(val_str)));
        ctx.set_field(this, BI_FIELD_SIGNUM, Value::Int(signum));
    }
}

fn native_bi_init_string_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => "0".to_string(),
    };
    let radix = match args.get(2) {
        Some(Value::Int(r)) => *r,
        _ => 10,
    };
    // Unlike `toString(int)` — which IGNORES a bad radix and uses 10 — the
    // `BigInteger(String, int)` CONSTRUCTOR throws. Measured on real JDK 25:
    // radix 0, 1, -1, 37, 40 and Integer.MIN_VALUE all raise
    // `NumberFormatException: Radix out of range`.
    //
    // This guard also closes a panic: the old body called
    // `i128::from_str_radix(abs, radix as u32)`, and `from_str_radix` panics
    // when the radix is outside 2..=36 (a negative radix widened to a huge
    // `u32` besides). An ordinary `new BigInteger(s, 40)` from Java aborted
    // the VM instead of throwing.
    if !(2..=36).contains(&radix) {
        return Err(RuntimeError::NumberFormatException {
            message: "Radix out of range".to_string(),
        }
        .into());
    }
    // Convert from given radix to decimal
    let decimal = if radix == 10 {
        s
    } else {
        // Arbitrary precision. The old body narrowed through `i128` and then
        // `.unwrap_or(0)`, so any value past `i128::MAX` — and any malformed
        // string — silently became 0 where the JDK either keeps every digit
        // or throws.
        let (neg, abs) = match s.strip_prefix('+') {
            // The JDK accepts a leading `+`; `bi_parse_sign` only knows `-`.
            Some(rest) => (false, rest),
            None => bi_parse_sign(&s),
        };
        if abs.is_empty() {
            return Err(RuntimeError::NumberFormatException {
                message: "Zero length BigInteger".to_string(),
            }
            .into());
        }
        let base = crate::bigint::BigInt::from_le_words(false, vec![radix as u32]);
        let mut acc = crate::bigint::BigInt::zero();
        for ch in abs.chars() {
            let Some(d) = ch.to_digit(radix as u32) else {
                return Err(RuntimeError::NumberFormatException {
                    message: format!("For input string: \"{s}\" under radix {radix}"),
                }
                .into());
            };
            acc = acc
                .mul(&base)
                .add(&crate::bigint::BigInt::from_le_words(false, vec![d]));
        }
        if neg {
            acc.neg_value().to_decimal()
        } else {
            acc.to_decimal()
        }
    };
    bi_write_into(ctx, this, &decimal);
    Ok(None)
}

fn native_bi_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(l)) => *l,
        _ => 0,
    };
    // Fresh limb-backed object, no decimal round-trip. The JDK's -16..16
    // constant cache is an optional optimization (the spec allows fresh
    // instances), and value-equality is what all JDK bytecode relies on.
    let result = bi_alloc_int(ctx, &bigint_from_i64(v));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb arithmetic (bigint::BigInt) — O(words), no decimal
    // round-trip. The decimal bi_add_str path this replaces paid an O(n^2)
    // words->decimal->words conversion on every op, which dominated BC EC
    // field arithmetic over generic Fp curves (~136ms/scalar-mult interpreted).
    let a = bi_read_int(ctx, this);
    let b = bi_read_int(ctx, other);
    let result = bi_alloc_int(ctx, &a.add(&b));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_subtract(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb arithmetic — see native_bi_add.
    let a = bi_read_int(ctx, this);
    let b = bi_read_int(ctx, other);
    let result = bi_alloc_int(ctx, &a.sub(&b));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb multiply (bigint::BigInt::mul, schoolbook O(words^2)) —
    // replaces the O(digits^2) decimal bi_mul_str plus two O(n^2) decimal
    // conversions. This is the hot field-multiply for generic Fp EC curves.
    let a = bi_read_int(ctx, this);
    let b = bi_read_int(ctx, other);
    let result = bi_alloc_int(ctx, &a.mul(&b));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_divide(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bi_read(ctx, this);
    let b = bi_read(ctx, other);
    if b == "0" {
        return Err(RuntimeError::ArithmeticException {
            message: "BigInteger divide by zero".to_string(),
        }
        .into());
    }
    let result = bi_alloc(ctx, &bi_div_str(&a, &b));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_mod(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bi_read(ctx, this);
    let b = bi_read(ctx, other);
    let result = bi_alloc(ctx, &bi_mod_str(&a, &b));
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_negate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb path — sign flip only, no decimal round-trip.
    let a = bi_read_int(ctx, this);
    let result = bi_alloc_int(ctx, &a.neg_value());
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bi_read(ctx, this);
    let abs = if let Some(rest) = a.strip_prefix('-') {
        rest.to_string()
    } else {
        a
    };
    let result = bi_alloc(ctx, &abs);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bi_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Word-based limb compare — the decimal path paid two O(words^2)
    // mag->decimal conversions per call, and compareTo is on the hot path of
    // both BC field arithmetic and Lucene's TestUtil.nextLong.
    let a = bi_read_int(ctx, this);
    let b = bi_read_int(ctx, other);
    Ok(Some(Value::Int(match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })))
}

fn native_bi_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // BigInt is normalized (no trailing zero limbs, canonical zero), so
    // structural equality is value equality.
    let a = bi_read_int(ctx, this);
    let b = bi_read_int(ctx, other);
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

fn native_bi_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Limb read + chunked (10^9-per-division) decimal conversion — same
    // output as the old digit-at-a-time `bi_read`, one word-division per 9
    // digits instead of one per digit.
    let s = bi_read_int(ctx, this).to_decimal();
    let java_str = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// Render an unsigned `u32` in `radix` with no zero padding (`"0"` for zero).
///
/// `radix` must already be in `2..=36`; `char::from_digit` produces the
/// lowercase digits the JDK uses.
fn bi_radix_digits(mut v: u32, radix: u32) -> String {
    if v == 0 {
        return "0".to_string();
    }
    let mut buf: Vec<char> = Vec::new();
    while v > 0 {
        buf.push(char::from_digit(v % radix, radix).unwrap_or('?'));
        v /= radix;
    }
    buf.into_iter().rev().collect()
}

/// Render a limb-based [`crate::bigint::BigInt`] in `radix`, exactly as
/// `java.math.BigInteger.toString(int)` does: SIGN-MAGNITUDE (a leading `-`,
/// never a two's-complement bit pattern), lowercase digits, no leading zeros,
/// `"0"` for zero.
///
/// `radix` must be in `2..=36` — callers substitute 10 for anything else, per
/// the JDK contract.
///
/// The loop mirrors `BigInt::to_decimal`: repeated short division of the
/// magnitude by the largest power of `radix` that still fits in a `u32`, so
/// each pass yields `chunk_digits` digits at once. Arbitrary precision — the
/// value is NEVER narrowed to a machine integer.
fn bi_to_radix_string(v: &crate::bigint::BigInt, radix: u32) -> String {
    debug_assert!((2..=36).contains(&radix));
    if v.is_zero() {
        return "0".to_string();
    }
    let mut chunk: u64 = radix as u64;
    let mut chunk_digits: usize = 1;
    while chunk * (radix as u64) <= u32::MAX as u64 {
        chunk *= radix as u64;
        chunk_digits += 1;
    }
    let mut work: Vec<u32> = v.mag_le().to_vec();
    let mut chunks: Vec<u32> = Vec::new();
    while !work.is_empty() {
        let mut rem: u64 = 0;
        for i in (0..work.len()).rev() {
            let cur = (rem << 32) | (work[i] as u64);
            work[i] = (cur / chunk) as u32;
            rem = cur % chunk;
        }
        while work.last() == Some(&0) {
            work.pop();
        }
        chunks.push(rem as u32);
    }
    let mut out = String::new();
    if v.is_neg() {
        out.push('-');
    }
    for (i, c) in chunks.iter().rev().enumerate() {
        let digits = bi_radix_digits(*c, radix);
        if i > 0 {
            // Interior chunks are zero-padded to the full chunk width; only
            // the most significant chunk may be short.
            for _ in digits.len()..chunk_digits {
                out.push('0');
            }
        }
        out.push_str(&digits);
    }
    out
}

/// Registered exactly once, by the synthetic-jdk-only
/// `register_biginteger_natives` — the lean real-JDK variant
/// (`register_biginteger_arithmetic_overrides`) deliberately registers only
/// `toString()`, so in real-JDK mode `BigInteger.toString(int)` stays on JDK
/// bytecode. Verified by grepping every `register(.., "toString",
/// "(I)Ljava/lang/String;", ..)` in the workspace; do the same before assuming
/// this body is the one that runs.
fn native_bi_to_string_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let radix = match args.get(1) {
        Some(Value::Int(r)) => *r,
        _ => 10,
    };
    // JDK contract (measured against real JDK 25): a radix outside
    // `Character.MIN_RADIX`..`Character.MAX_RADIX` is IGNORED and radix 10 is
    // used instead — `BigInteger.toString(int)` does NOT throw. Verified for
    // radix 0, 1, -1, 37, 40, Integer.MIN_VALUE and Integer.MAX_VALUE. The
    // substitution is shared with `Integer`/`Long.toString(…, int)`.
    let radix: u32 = crate::java_radix_or_ten(radix);
    if radix == 10 {
        // RBIGDEC.1 — bi_read returns the decimal already; allocate a fresh
        // Java string instead of returning the raw slot 0 (which in real-JDK
        // mode is signum:I, not the value string).
        let a = bi_read(ctx, this);
        let result = ctx.create_string(&a);
        return Ok(Some(Value::Object(Some(result))));
    }
    // Sign-magnitude over the limbs. The previous body narrowed to `i128` and
    // then used `format!("{:x}"/"{:o}"/"{:b}")`, which (a) printed the
    // two's-complement pattern for negatives (`BigInteger.valueOf(-1)
    // .toString(16)` answered 32 `f`s where the JDK says "-1"), (b) silently
    // answered "0" for any value past `i128`, and (c) fell through to the
    // DECIMAL string for every radix in 3..=36 other than 8 and 16.
    let v = bi_read_int(ctx, this);
    let s = bi_to_radix_string(&v, radix);
    let result = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(result))))
}

/// Read the low `n_words` magnitude words of a BigInteger as an unsigned
/// little-endian u128 (least-significant word first). Returns `(signum, lo)`
/// where `lo` packs the lowest `n_words * 32` bits of the absolute value.
///
/// `n_words` must be `<= 4`. For real-JDK layout (`mag:[I` big-endian, base
/// 2^32) the lowest word lives at `mag[mag.length - 1]`. For the synthetic
/// fallback layout we go through `bi_read` and decimal-parse — slow, but only
/// hit in synthetic-jdk mode.
fn bi_low_bits(ctx: &dyn NativeContext, this: ObjectRef, n_words: usize) -> (i32, u128) {
    debug_assert!(n_words <= 4);
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let signum = match ctx.get_field(this, sig_i) {
            Value::Int(s) => s,
            _ => 0,
        };
        if signum == 0 {
            return (0, 0);
        }
        let mag = match ctx.get_field(this, mag_i) {
            Value::Object(Some(o)) => o,
            _ => return (signum, 0),
        };
        let len = ctx.array_length(mag);
        if len == 0 {
            return (signum, 0);
        }
        // mag[] is big-endian base 2^32, so mag[len-1-k] is the k'th word
        // (counting from the LSB). Pack up to `n_words` low words into u128.
        let mut lo: u128 = 0;
        for k in 0..n_words {
            if k >= len {
                break;
            }
            let idx = len - 1 - k;
            let w = match ctx.get_array_element(mag, idx) {
                Value::Int(v) => v as u32,
                _ => 0,
            };
            lo |= (w as u128) << (32 * k);
        }
        (signum, lo)
    } else {
        // Synthetic-stub layout: fall back to decimal-string parse.
        let a = bi_read(ctx, this);
        let (neg, abs) = bi_parse_sign(&a);
        // Repeatedly mod by 2^(n_words*32) via decimal arithmetic — we only
        // need the low bits. For small magnitudes the string itself parses;
        // for big ones reduce step-by-step.
        let modulus_bits = n_words * 32;
        let mut decimal = abs.to_string();
        let mut lo: u128 = 0;
        // Build lo by extracting low 32 bits at a time.
        for k in 0..n_words {
            if decimal == "0" {
                break;
            }
            // word = decimal mod 2^32
            let mut word: u64 = 0;
            for ch in decimal.chars() {
                let d = ch.to_digit(10).unwrap_or(0) as u64;
                word = (word * 10 + d) & 0xFFFF_FFFF;
            }
            lo |= (word as u128) << (32 * k);
            // decimal = decimal / 2^32 (32 shift-rights via /2)
            for _ in 0..32 {
                decimal = bi_div_unsigned(&decimal, "2");
                if decimal == "0" {
                    break;
                }
            }
        }
        let _ = modulus_bits;
        let signum = if abs == "0" {
            0
        } else if neg {
            -1
        } else {
            1
        };
        (signum, lo)
    }
}

/// Compute the low `bits` two's-complement bits of a BigInteger.
/// For positive values: return the low `bits` bits of the magnitude.
/// For negative values: return the low `bits` bits of `~(mag-1) + 1` — i.e.
/// negate the magnitude as if it were an infinite-precision two's-complement
/// integer, then truncate. `bits` must be 32, 64, or 128.
fn bi_low_twos_complement(ctx: &dyn NativeContext, this: ObjectRef, bits: u32) -> u128 {
    debug_assert!(bits == 32 || bits == 64 || bits == 128);
    let n_words = (bits / 32) as usize;
    let (signum, mag_lo) = bi_low_bits(ctx, this, n_words);
    let mask: u128 = if bits == 128 {
        u128::MAX
    } else {
        (1u128 << bits) - 1
    };
    if signum >= 0 {
        mag_lo & mask
    } else {
        // Two's-complement negation of the magnitude, but we only kept the
        // low words. The high (truncated) magnitude words affect the result
        // only if they were nonzero AND we'd be propagating a borrow into
        // the kept range. Equivalent: if mag_lo == 0 across all kept words,
        // any nonzero high word makes the truncated two's-complement still 0
        // (since negating 2^k gives ...1110...0, low k bits all zero).
        // Otherwise the kept range's two's-complement is `(~mag_lo + 1) & mask`.
        // Note: for the case where mag has more words than we kept and
        // mag_lo != 0, the high-word contribution to the borrow is already
        // captured by working modulo 2^bits.
        ((!mag_lo).wrapping_add(1)) & mask
    }
}

fn native_bi_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // BigInteger.intValue() returns the low 32 bits as a signed int — i.e.
    // truncate the two's-complement representation to 32 bits. The old
    // implementation went through `bi_read` + `i32::parse` which silently
    // returned 0 for any value outside `[i32::MIN, i32::MAX]`, breaking
    // every caller that pulls 32-bit chunks out of a wide BigInteger (e.g.
    // BouncyCastle's `Nat.fromBigInteger`).
    let low = bi_low_twos_complement(ctx, this, 32) as u32;
    Ok(Some(Value::Int(low as i32)))
}

fn native_bi_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Same fix as `intValue` — return the low 64 two's-complement bits.
    let low = bi_low_twos_complement(ctx, this, 64) as u64;
    Ok(Some(Value::Long(low as i64)))
}

fn native_bi_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let a = bi_read(ctx, this);
    let val: f64 = a.parse().unwrap_or(0.0);
    Ok(Some(Value::Double(val)))
}

fn native_bi_float_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let a = bi_read(ctx, this);
    let val: f32 = a.parse().unwrap_or(0.0);
    Ok(Some(Value::Float(val)))
}

fn native_bi_signum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // RBIGDEC.1 — read from the real-JDK `signum:I` slot when available.
    // Falls back to the synthetic-stub slot 1.  We deliberately do not use
    // `bi_read` + sign-of-string here because that path requires reading
    // `mag[]` and is overkill for a single-int read.
    let sig_idx = bi_layout(ctx).map(|(s, _)| s).unwrap_or(BI_FIELD_SIGNUM);
    let signum = match ctx.get_field(this, sig_idx) {
        Value::Int(s) => s,
        _ => 0,
    };
    Ok(Some(Value::Int(signum)))
}

fn native_bi_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = bi_read(ctx, this);
    let mut h: i32 = 0;
    for b in a.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

fn native_bi_pow(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::bigint::BigInt;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let exp = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // JDK: pow(negative) throws; pow(0) == ONE (even for a zero base).
    if exp < 0 {
        return Err(RuntimeError::ArithmeticException {
            message: "Negative exponent".to_string(),
        }
        .into());
    }
    // Square-and-multiply on binary limbs. The old implementation was an
    // O(exp) loop of decimal-string schoolbook multiplies with a full
    // BigInteger heap allocation per step — `new BigDecimal(double)` runs
    // 5^52 through here (real-JDK bytecode delegates to BigInteger.pow), so
    // that O(exp) loop was a measured ~114us per BigDecimal(double) ctor in
    // Lucene's TestUtil.nextLong hot path (ES codec/doc-values test hangs).
    let base = bi_read_int(ctx, this);
    let mut result = BigInt::from_decimal("1");
    let mut sq = base;
    let mut e = exp as u32;
    while e > 0 {
        if e & 1 == 1 {
            result = result.mul(&sq);
        }
        e >>= 1;
        if e > 0 {
            sq = sq.mul(&sq);
        }
    }
    let obj = bi_alloc_int(ctx, &result);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_bi_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bi_read(ctx, this);
    let b = bi_read(ctx, other);
    if bi_compare(&a, &b) >= 0 {
        Ok(Some(args[0]))
    } else {
        Ok(Some(args[1]))
    }
}

fn native_bi_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bi_read(ctx, this);
    let b = bi_read(ctx, other);
    if bi_compare(&a, &b) <= 0 {
        Ok(Some(args[0]))
    } else {
        Ok(Some(args[1]))
    }
}

/// RBIGDEC.1 — Resolve the real-JDK BigDecimal field layout if available.
///
/// Returns the slot indices for `(intVal, scale, precision, intCompact)`
/// when the JDK class is loaded.  None ⇒ synthetic-stub fallback.
fn bd_layout(ctx: &dyn NativeContext) -> Option<(usize, usize, usize, usize)> {
    let iv = ctx.resolve_field_index("java/math/BigDecimal", "intVal")?;
    let sc = ctx.resolve_field_index("java/math/BigDecimal", "scale")?;
    let pr = ctx.resolve_field_index("java/math/BigDecimal", "precision")?;
    let ic = ctx.resolve_field_index("java/math/BigDecimal", "intCompact")?;
    Some((iv, sc, pr, ic))
}

/// Recover the unscaled-integer digits from a plain decimal rendering +
/// scale, and the JDK-true precision of that unscaled value.
///
/// For `scale >= 0` the unscaled is simply the rendering with the '.'
/// removed. For `scale < 0` the plain rendering (from `apply_scale` /
/// `bd_read`) has the |scale| trailing zeros BAKED IN — deriving the
/// unscaled by dot-stripping alone would inflate the value by 10^|scale|
/// (unscaled 5, scale -3 rendered "5000" read back as 5000×10³).
///
/// Precision is the JDK's: significant digits of the unscaled value — sign
/// ignored, leading zeros never counted ("0.05" → 1, NOT 3), zero → 1.
/// The old `value.replace(['-','.'],"").len()` overcounted exactly those
/// leading zeros, which (together with the raw lazy-0 slot in
/// `native_bd_precision`) fed real `compareTo`'s adjusted-exponent quick
/// path garbage and produced strict comparison CYCLES (a<b<c<a) over H2
/// DECIMAL values — the queryGroup GROUP-BY pseudo-hang.
fn bd_unscaled_and_precision(value: &str, scale: i32) -> (String, i32) {
    let mut unscaled = value.replace('.', "");
    if scale < 0 {
        let n = (-scale) as usize;
        let abs_len = unscaled.strip_prefix('-').map_or(unscaled.len(), str::len);
        if abs_len > n
            && unscaled.as_bytes()[unscaled.len() - n..unscaled.len()]
                .iter()
                .all(|&b| b == b'0')
        {
            unscaled.truncate(unscaled.len() - n);
        }
    }
    let abs = unscaled.strip_prefix('-').unwrap_or(&unscaled);
    let trimmed = abs.trim_start_matches('0');
    let precision = if trimmed.is_empty() {
        1
    } else {
        trimmed.len() as i32
    };
    (unscaled, precision)
}

fn bd_alloc(ctx: &mut dyn NativeContext, value: &str, scale: i32) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "java/math/BigDecimal", 3);
    // GC-SAFETY (use-after-move — see `bi_alloc`/`bi_alloc_int`): pin `obj`
    // across the `bi_alloc` / `create_string` allocations below, which can
    // trigger a minor GC that relocates the not-yet-rooted `obj`. Each branch
    // re-reads the forwarded ref before its `set_field`s, and the returned ref
    // is the forwarded one. Without this, a GC inside `bi_alloc` leaves `obj`
    // stale → `set_field` corrupts the heap (H2 TestScript BigDecimal SEGV).
    let h = ctx.pin_native_root(obj);
    let (unscaled_str, precision) = bd_unscaled_and_precision(value, scale);
    if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
        // Real-JDK layout: build the value as the scaled unscaled-integer
        // representation.  `value` may include a decimal point (e.g.
        // "1.5", scale=1 → unscaled=15).
        // Try a fast i64 path; fall back to inflated BigInteger.
        let bi_class_id = ctx.class_id_by_name("java/math/BigInteger");
        let int_compact = unscaled_str.parse::<i64>().unwrap_or(BD_INFLATED);
        if int_compact == BD_INFLATED {
            // Value out of i64 range (or literal `Long.MIN_VALUE`, which we
            // treat as inflated to keep the sentinel pure).  Allocate an
            // intVal BigInteger.
            let bi = bi_alloc(ctx, &unscaled_str);
            let obj = ctx.read_native_pin(h, obj);
            ctx.set_field(obj, iv_i, Value::Object(Some(bi)));
            ctx.set_field(obj, ic_i, Value::Long(BD_INFLATED));
            ctx.set_field(obj, sc_i, Value::Int(scale));
            ctx.set_field(obj, pr_i, Value::Int(precision));
        } else {
            // Compact path: leave `intVal` null (or, when non-null, the JDK
            // expects it to mirror `intCompact`).  Allocate a backing
            // BigInteger so reflective reads of `intVal` still see a real
            // object — matches HotSpot's behaviour for `BigDecimal.ONE`
            // where `intVal != null` even though `intCompact == 1`.
            let bi = if bi_class_id.is_some() {
                Some(bi_alloc(ctx, &unscaled_str))
            } else {
                None
            };
            let obj = ctx.read_native_pin(h, obj);
            ctx.set_field(obj, iv_i, Value::Object(bi));
            ctx.set_field(obj, ic_i, Value::Long(int_compact));
            ctx.set_field(obj, sc_i, Value::Int(scale));
            ctx.set_field(obj, pr_i, Value::Int(precision));
        }
    } else {
        let s = ctx.create_string(value);
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BD_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BD_FIELD_SCALE, Value::Int(scale));
        ctx.set_field(obj, BD_FIELD_PRECISION, Value::Int(precision));
    }
    let obj = ctx.read_native_pin(h, obj);
    ctx.unpin_native_roots(h);
    obj
}

pub(crate) fn register_bigdecimal_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: BigDecimal spec-exact arithmetic/factories → Intrinsic.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    let bd = "java/math/BigDecimal";
    registry.register(bd, "<init>", "(Ljava/lang/String;)V", native_bd_init_string);
    registry.register(bd, "<init>", "(D)V", native_bd_init_double);
    registry.register(bd, "<init>", "(I)V", native_bd_init_int);
    registry.register(bd, "<init>", "(J)V", native_bd_init_long);
    registry.register(
        bd,
        "valueOf",
        "(J)Ljava/math/BigDecimal;",
        native_bd_value_of_long,
    );
    registry.register(
        bd,
        "valueOf",
        "(D)Ljava/math/BigDecimal;",
        native_bd_value_of_double,
    );
    registry.register(
        bd,
        "add",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_add,
    );
    registry.register(
        bd,
        "subtract",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_subtract,
    );
    registry.register(
        bd,
        "multiply",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_multiply,
    );
    registry.register(
        bd,
        "divide",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_divide,
    );
    registry.register(
        bd,
        "divide",
        "(Ljava/math/BigDecimal;II)Ljava/math/BigDecimal;",
        native_bd_divide_scale,
    );
    registry.register(
        bd,
        "compareTo",
        "(Ljava/math/BigDecimal;)I",
        native_bd_compare_to,
    );
    registry.register(bd, "equals", "(Ljava/lang/Object;)Z", native_bd_equals);
    registry.register(bd, "toString", "()Ljava/lang/String;", native_bd_to_string);
    registry.register(
        bd,
        "toPlainString",
        "()Ljava/lang/String;",
        native_bd_to_string,
    );
    registry.register(bd, "intValue", "()I", native_bd_int_value);
    registry.register(bd, "longValue", "()J", native_bd_long_value);
    registry.register(bd, "doubleValue", "()D", native_bd_double_value);
    registry.register(bd, "floatValue", "()F", native_bd_float_value);
    registry.register(
        bd,
        "toBigInteger",
        "()Ljava/math/BigInteger;",
        native_bd_to_big_integer,
    );
    registry.register(bd, "scale", "()I", native_bd_scale);
    registry.register(bd, "precision", "()I", native_bd_precision);
    registry.register(bd, "negate", "()Ljava/math/BigDecimal;", native_bd_negate);
    registry.register(bd, "abs", "()Ljava/math/BigDecimal;", native_bd_abs);
    registry.register(bd, "signum", "()I", native_bd_signum);
    registry.register(
        bd,
        "setScale",
        "(I)Ljava/math/BigDecimal;",
        native_bd_set_scale,
    );
    registry.register(
        bd,
        "setScale",
        "(II)Ljava/math/BigDecimal;",
        native_bd_set_scale_rounding,
    );
    registry.register(
        bd,
        "stripTrailingZeros",
        "()Ljava/math/BigDecimal;",
        native_bd_strip_zeros,
    );
    registry.register(bd, "hashCode", "()I", native_bd_hash_code);
    registry.register(bd, "ZERO", "()Ljava/math/BigDecimal;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "0", 0)))))
    });
    registry.register(bd, "ONE", "()Ljava/math/BigDecimal;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "1", 0)))))
    });
    registry.register(bd, "TEN", "()Ljava/math/BigDecimal;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "10", 0)))))
    });
    registry.set_category(__prev_cat);
}

/// Read a `BigDecimal`'s `(unscaled-digits, scale)` in real-JDK layout, or
/// `None` for the synthetic-stub layout (which stores a ready decimal string).
fn bd_read_parts(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(String, i32)> {
    let (iv_i, sc_i, _pr_i, ic_i) = bd_layout(ctx)?;
    let scale = match ctx.get_field(this, sc_i) {
        Value::Int(s) => s,
        _ => 0,
    };
    let int_compact = match ctx.get_field(this, ic_i) {
        Value::Long(l) => l,
        _ => BD_INFLATED,
    };
    let unscaled = if int_compact != BD_INFLATED {
        int_compact.to_string()
    } else {
        match ctx.get_field(this, iv_i) {
            // Chunked limb→decimal conversion (10^9 per division) — the
            // digit-at-a-time `bi_read` path costs one long-division per
            // digit and shows up on every toString/toPlainString of an
            // inflated value.
            Value::Object(Some(bi)) => bi_read_int(ctx, bi).to_decimal(),
            _ => "0".to_string(),
        }
    };
    Some((unscaled, scale))
}

/// Plain (`toPlainString`-style, round-trippable) rendering — also the internal
/// arithmetic form.
fn bd_read(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    if let Some((unscaled, scale)) = bd_read_parts(ctx, this) {
        return apply_scale(&unscaled, scale);
    }
    // Synthetic-stub fallback — the value is already a decimal string.
    match ctx.get_field(this, BD_FIELD_VALUE) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "0".to_string()),
        _ => "0".to_string(),
    }
}

/// Canonical `toString()` rendering (scientific notation when appropriate).
fn bd_read_canonical(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    if let Some((unscaled, scale)) = bd_read_parts(ctx, this) {
        return bd_layout_chars(&unscaled, scale);
    }
    match ctx.get_field(this, BD_FIELD_VALUE) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "0".to_string()),
        _ => "0".to_string(),
    }
}

/// Read the `scale` int from a `BigDecimal`, picking the layout-correct slot.
fn bd_scale_of(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    let idx = bd_layout(ctx)
        .map(|(_, sc, _, _)| sc)
        .unwrap_or(BD_FIELD_SCALE);
    match ctx.get_field(this, idx) {
        Value::Int(s) => s,
        _ => 0,
    }
}

/// Read the `precision` int from a `BigDecimal`, picking the layout slot.
fn bd_precision_of(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    let idx = bd_layout(ctx)
        .map(|(_, _, pr, _)| pr)
        .unwrap_or(BD_FIELD_PRECISION);
    match ctx.get_field(this, idx) {
        Value::Int(p) => p,
        _ => 0,
    }
}

/// `BigDecimal.toString()` canonical layout (java.math.BigDecimal.toString):
/// scientific notation when the scale is negative OR the adjusted exponent is
/// `< -6`; plain decimal otherwise. (`toPlainString()` keeps the plain
/// `apply_scale` rendering, and `apply_scale` is also used unchanged for the
/// internal round-trippable arithmetic form.)
fn bd_layout_chars(unscaled: &str, scale: i32) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let (neg, abs) = if let Some(stripped) = unscaled.strip_prefix('-') {
        (true, stripped.to_string())
    } else {
        (false, unscaled.to_string())
    };
    let sign = if neg { "-" } else { "" };
    let coeff_len = abs.len() as i64;
    // Adjusted exponent of the leftmost digit: (digits - 1) - scale.
    let adjusted = coeff_len - 1 - scale as i64;
    if scale > 0 && adjusted >= -6 {
        // Plain decimal (positive scale, not too small).
        let s = scale as usize;
        if abs.len() > s {
            let split = abs.len() - s;
            format!("{}{}.{}", sign, &abs[..split], &abs[split..])
        } else {
            let pad = s - abs.len();
            format!("{}0.{}{}", sign, "0".repeat(pad), abs)
        }
    } else {
        // Scientific notation: one digit before the point, signed exponent.
        let mantissa = if abs.len() == 1 {
            abs.clone()
        } else {
            format!("{}.{}", &abs[..1], &abs[1..])
        };
        let exp_sign = if adjusted >= 0 { "+" } else { "-" };
        format!("{}{}E{}{}", sign, mantissa, exp_sign, adjusted.abs())
    }
}

fn native_bd_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let s = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => "0".to_string(),
    };
    let scale = s.find('.').map(|p| (s.len() - p - 1) as i32).unwrap_or(0);
    bd_write_into(ctx, this, &s, scale);
    Ok(None)
}

/// Populate an existing `BigDecimal` instance from a decimal string + scale.
/// Picks the layout (real-JDK intVal/scale/precision/intCompact vs. legacy
/// synthetic value/scale/precision) automatically.
fn bd_write_into(ctx: &mut dyn NativeContext, this: ObjectRef, value: &str, scale: i32) {
    // GC-SAFETY (use-after-move — see `bi_alloc`): pin `this` across the
    // `bi_alloc` / `create_string` allocations, which can trigger a minor GC
    // that relocates it. Re-read the forwarded ref before the `set_field`s so we
    // initialize the LIVE copy of the receiver, not a stale (reused) slot.
    let h = ctx.pin_native_root(this);
    let (unscaled_str, precision) = bd_unscaled_and_precision(value, scale);
    if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
        let int_compact = unscaled_str.parse::<i64>().unwrap_or(BD_INFLATED);
        let bi = bi_alloc(ctx, &unscaled_str);
        let ic = if int_compact == BD_INFLATED {
            BD_INFLATED
        } else {
            int_compact
        };
        let this = ctx.read_native_pin(h, this);
        ctx.set_field(this, iv_i, Value::Object(Some(bi)));
        ctx.set_field(this, ic_i, Value::Long(ic));
        ctx.set_field(this, sc_i, Value::Int(scale));
        ctx.set_field(this, pr_i, Value::Int(precision));
    } else {
        let val_str = ctx.create_string(value);
        let this = ctx.read_native_pin(h, this);
        ctx.set_field(this, BD_FIELD_VALUE, Value::Object(Some(val_str)));
        ctx.set_field(this, BD_FIELD_SCALE, Value::Int(scale));
        ctx.set_field(this, BD_FIELD_PRECISION, Value::Int(precision));
    }
    ctx.unpin_native_roots(h);
}

/// Populate an existing real-layout `BigDecimal` from an exact
/// `(unscaled, scale, precision)` triple — the write-into twin of
/// `bd_alloc_bigint` (same compact/inflated split, same lazy-`precision`
/// convention when the caller passes 0). Falls back to the decimal-string
/// `bd_write_into` for the synthetic layout.
fn bd_write_into_bigint(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    unscaled: &crate::bigint::BigInt,
    scale: i32,
    precision: i32,
) {
    if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
        let le = unscaled.mag_le();
        let compact: Option<i64> = if le.len() <= 2 {
            let mag = (le.first().copied().unwrap_or(0) as u64)
                | ((le.get(1).copied().unwrap_or(0) as u64) << 32);
            if mag <= i64::MAX as u64 {
                Some(if unscaled.is_neg() {
                    -(mag as i64)
                } else {
                    mag as i64
                })
            } else {
                None
            }
        } else {
            None
        };
        if let Some(ic) = compact {
            ctx.set_field(this, iv_i, Value::Object(None));
            ctx.set_field(this, ic_i, Value::Long(ic));
            ctx.set_field(this, sc_i, Value::Int(scale));
            ctx.set_field(this, pr_i, Value::Int(precision));
            return;
        }
        // Inflated: pin `this` across the BigInteger allocation (GC-SAFETY —
        // see `bd_write_into`).
        let h = ctx.pin_native_root(this);
        let bi = bi_alloc_int(ctx, unscaled);
        let this = ctx.read_native_pin(h, this);
        ctx.set_field(this, iv_i, Value::Object(Some(bi)));
        ctx.set_field(this, ic_i, Value::Long(BD_INFLATED));
        ctx.set_field(this, sc_i, Value::Int(scale));
        ctx.set_field(this, pr_i, Value::Int(precision));
        ctx.unpin_native_roots(h);
        return;
    }
    let value = apply_scale(&unscaled.to_decimal(), scale);
    bd_write_into(ctx, this, &value, scale);
}

fn native_bd_init_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::bigint::BigInt;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let d = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Exact JDK `BigDecimal(double)` semantics: the value is the double's
    // EXACT binary expansion (`0.1` → the 55-digit decimal), never the
    // shortest round-trip rendering `format!` produces. Mirrors the real
    // ctor bytecode: sign/exponent/significand decomposition, normalize the
    // significand to odd, then unscaled = sig<<exp (exp>0) or sig*5^-exp
    // with scale=-exp (exp<0). Registered as a real-JDK override because
    // constructors are JIT-banned (skip_list A1.4) so the real ctor runs
    // interpreted forever — it dominated Lucene TestUtil.nextLong's
    // large-range branch even after `BigInteger.pow` went native.
    if !d.is_finite() {
        return Err(RuntimeError::NumberFormatException {
            message: "Infinite or NaN".to_string(),
        }
        .into());
    }
    let bits = d.to_bits();
    let neg = (bits >> 63) != 0;
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mut sig, mut exp) = if biased == 0 {
        (frac << 1, -1075i32)
    } else {
        (frac | (1u64 << 52), biased - 1075)
    };
    if sig == 0 {
        // JDK: intVal = BigInteger.ZERO, intCompact = 0, scale = 0,
        // precision = 1 (also covers -0.0).
        let h = ctx.pin_native_root(this);
        let zero = bi_alloc_int(ctx, &BigInt::zero());
        let this = ctx.read_native_pin(h, this);
        if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
            ctx.set_field(this, iv_i, Value::Object(Some(zero)));
            ctx.set_field(this, ic_i, Value::Long(0));
            ctx.set_field(this, sc_i, Value::Int(0));
            ctx.set_field(this, pr_i, Value::Int(1));
        } else {
            let s = ctx.create_string("0");
            ctx.set_field(this, BD_FIELD_VALUE, Value::Object(Some(s)));
            ctx.set_field(this, BD_FIELD_SCALE, Value::Int(0));
            ctx.set_field(this, BD_FIELD_PRECISION, Value::Int(1));
        }
        ctx.unpin_native_roots(h);
        return Ok(None);
    }
    while sig & 1 == 0 {
        sig >>= 1;
        exp += 1;
    }
    let mag = BigInt::from_le_words(false, vec![sig as u32, (sig >> 32) as u32]);
    let (unscaled, scale) = if exp == 0 {
        (mag, 0)
    } else if exp > 0 {
        (mag.shl(exp as u32), 0)
    } else {
        (mag.mul(&bigint_pow5((-exp) as u32)), -exp)
    };
    let unscaled = if neg { unscaled.neg_value() } else { unscaled };
    bd_write_into_bigint(ctx, this, &unscaled, scale, 0);
    Ok(None)
}

/// `BigDecimal(BigInteger)` — intVal = the argument (kept only when
/// inflated, like the JDK's `compactValFor` split), scale 0, lazy precision.
/// Registered as a real-JDK override for the same ctor-JIT-ban reason as
/// `<init>(D)`.
fn native_bd_init_bigint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let bi_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
    };
    let v = bi_read_int(ctx, bi_obj);
    if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
        let le = v.mag_le();
        let compact: Option<i64> = if le.len() <= 2 {
            let mag = (le.first().copied().unwrap_or(0) as u64)
                | ((le.get(1).copied().unwrap_or(0) as u64) << 32);
            if mag <= i64::MAX as u64 {
                Some(if v.is_neg() {
                    -(mag as i64)
                } else {
                    mag as i64
                })
            } else {
                None
            }
        } else {
            None
        };
        match compact {
            Some(ic) => {
                ctx.set_field(this, iv_i, Value::Object(None));
                ctx.set_field(this, ic_i, Value::Long(ic));
            }
            None => {
                // Inflated: store the caller's BigInteger itself, like the
                // real ctor (no copy, no fresh allocation).
                ctx.set_field(this, iv_i, Value::Object(Some(bi_obj)));
                ctx.set_field(this, ic_i, Value::Long(BD_INFLATED));
            }
        }
        ctx.set_field(this, sc_i, Value::Int(0));
        ctx.set_field(this, pr_i, Value::Int(0));
        return Ok(None);
    }
    bd_write_into_bigint(ctx, this, &v, 0, 0);
    Ok(None)
}

fn native_bd_init_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let s = v.to_string();
    bd_write_into(ctx, this, &s, 0);
    Ok(None)
}

fn native_bd_init_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let v = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let s = v.to_string();
    bd_write_into(ctx, this, &s, 0);
    Ok(None)
}

fn native_bd_value_of_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(l)) => *l,
        _ => 0,
    };
    let result = bd_alloc(ctx, &v.to_string(), 0);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_value_of_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let d = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let s = format!("{}", d);
    let scale = s.find('.').map(|p| (s.len() - p - 1) as i32).unwrap_or(0);
    let result = bd_alloc(ctx, &s, scale);
    Ok(Some(Value::Object(Some(result))))
}

fn bd_unscaled_bigint(ctx: &dyn NativeContext, this: ObjectRef) -> (crate::bigint::BigInt, i32) {
    use crate::bigint::BigInt;
    let scale = bd_scale_of(ctx, this);
    if let Some((iv_i, _sc_i, _pr_i, ic_i)) = bd_layout(ctx) {
        let ic = match ctx.get_field(this, ic_i) {
            Value::Long(l) => l,
            _ => BD_INFLATED,
        };
        if ic != BD_INFLATED {
            return (bigint_from_i64(ic), scale);
        }
        if let Value::Object(Some(bi)) = ctx.get_field(this, iv_i) {
            return (bi_read_int(ctx, bi), scale);
        }
        return (BigInt::zero(), scale);
    }
    let s = match ctx.get_field(this, BD_FIELD_VALUE) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_else(|| "0".to_string()),
        _ => "0".to_string(),
    };
    (BigInt::from_decimal(&s.replace('.', "")), scale)
}

/// Construct a `BigDecimal` from an exact `(unscaled, scale)` pair (the inverse
/// of `bd_unscaled_bigint`).
///
/// Real-JDK layout: write `intCompact`/`intVal`/`scale` directly from the limb
/// value — no decimal rendering. `precision` is written as the JDK's lazy `0`
/// sentinel (real bytecode leaves it 0 too; `native_bd_precision` computes and
/// caches on demand), and `intVal` stays null on the compact path exactly as
/// `BigDecimal.valueOf(long, int)` leaves it — every JDK bytecode read goes
/// through `inflated()`, which handles null. The old implementation rendered
/// the value to a decimal string and re-parsed it (digit count, `i64` parse,
/// `decimal_to_mag_words`) on every arithmetic result.
fn bd_alloc_bigint(
    ctx: &mut dyn NativeContext,
    unscaled: &crate::bigint::BigInt,
    scale: i32,
) -> ObjectRef {
    if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
        // Compact iff |unscaled| <= i64::MAX (Long.MIN_VALUE is the INFLATED
        // sentinel, so exactly -2^63 must stay inflated, matching the JDK's
        // compactValFor).
        let le = unscaled.mag_le();
        let compact: Option<i64> = if le.len() <= 2 {
            let mag = (le.first().copied().unwrap_or(0) as u64)
                | ((le.get(1).copied().unwrap_or(0) as u64) << 32);
            if mag <= i64::MAX as u64 {
                Some(if unscaled.is_neg() {
                    -(mag as i64)
                } else {
                    mag as i64
                })
            } else {
                None
            }
        } else {
            None
        };
        let obj = alloc_concurrent_synthetic(ctx, "java/math/BigDecimal", 3);
        if let Some(ic) = compact {
            ctx.set_field(obj, iv_i, Value::Object(None));
            ctx.set_field(obj, ic_i, Value::Long(ic));
            ctx.set_field(obj, sc_i, Value::Int(scale));
            ctx.set_field(obj, pr_i, Value::Int(0));
            return obj;
        }
        // Inflated: allocate the backing BigInteger. Pin `obj` across that
        // allocation (GC-SAFETY — see `bd_alloc`).
        let h = ctx.pin_native_root(obj);
        let bi = bi_alloc_int(ctx, unscaled);
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, iv_i, Value::Object(Some(bi)));
        ctx.set_field(obj, ic_i, Value::Long(BD_INFLATED));
        ctx.set_field(obj, sc_i, Value::Int(scale));
        ctx.set_field(obj, pr_i, Value::Int(0));
        ctx.unpin_native_roots(h);
        return obj;
    }
    // Synthetic-stub layout: fall back to the decimal-string path.
    let value = apply_scale(&unscaled.to_decimal(), scale);
    bd_alloc(ctx, &value, scale)
}

fn native_bd_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Exact: result scale = max(sa, sb); rescale both unscaled to it, then add.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = sa.max(sb);
    let sum = bigint_mul_pow10(&ua, s - sa).add(&bigint_mul_pow10(&ub, s - sb));
    let result = bd_alloc_bigint(ctx, &sum, s);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_subtract(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Exact: result scale = max(sa, sb); rescale both unscaled to it, then subtract.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = sa.max(sb);
    let diff = bigint_mul_pow10(&ua, s - sa).sub(&bigint_mul_pow10(&ub, s - sb));
    let result = bd_alloc_bigint(ctx, &diff, s);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Exact: result scale = sa + sb; multiply the unscaled integers directly.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = sa + sb;
    let prod = ua.mul(&ub);
    let result = bd_alloc_bigint(ctx, &prod, s);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_divide(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a: f64 = bd_read(ctx, this).parse().unwrap_or(0.0);
    let b: f64 = bd_read(ctx, other).parse().unwrap_or(0.0);
    if b == 0.0 {
        return Err(RuntimeError::ArithmeticException {
            message: "BigDecimal divide by zero".to_string(),
        }
        .into());
    }
    let s = format!("{}", a / b);
    let scale = s.find('.').map(|p| (s.len() - p - 1) as i32).unwrap_or(0);
    let result = bd_alloc(ctx, &s, scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_divide_scale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_scale = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let a: f64 = bd_read(ctx, this).parse().unwrap_or(0.0);
    let b: f64 = bd_read(ctx, other).parse().unwrap_or(0.0);
    if b == 0.0 {
        return Err(RuntimeError::ArithmeticException {
            message: "BigDecimal divide by zero".to_string(),
        }
        .into());
    }
    let s = format!("{:.prec$}", a / b, prec = new_scale as usize);
    let result = bd_alloc(ctx, &s, new_scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a: f64 = bd_read(ctx, this).parse().unwrap_or(0.0);
    let b: f64 = bd_read(ctx, other).parse().unwrap_or(0.0);
    Ok(Some(Value::Int(
        a.partial_cmp(&b).map(|o| o as i32).unwrap_or(0),
    )))
}

fn native_bd_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = bd_read(ctx, this);
    let b = bd_read(ctx, other);
    let a_scale = bd_scale_of(ctx, this);
    let b_scale = bd_scale_of(ctx, other);
    Ok(Some(Value::Int(if a == b && a_scale == b_scale {
        1
    } else {
        0
    })))
}

fn native_bd_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // toString() uses the canonical layout (scientific notation when the scale
    // is negative or the adjusted exponent < -6); toPlainString() uses the
    // plain form (native_bd_to_plain_string).
    let s = bd_read_canonical(ctx, this);
    let java_str = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// `BigDecimal.toPlainString()` — never uses exponential notation.
fn native_bd_to_plain_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = bd_read(ctx, this);
    let java_str = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// The unscaled value with the fraction dropped (truncation toward zero) —
/// the integer part `BigDecimal.toBigInteger()` returns. Shared by
/// `toBigInteger`/`longValue`/`intValue`.
fn bd_truncated_bigint(ctx: &dyn NativeContext, this: ObjectRef) -> crate::bigint::BigInt {
    use crate::bigint::BigInt;
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    if scale == 0 {
        return u;
    }
    if scale < 0 {
        return bigint_mul_pow10(&u, -scale);
    }
    let mut divisor_dec = String::with_capacity(scale as usize + 1);
    divisor_dec.push('1');
    divisor_dec.push_str(&"0".repeat(scale as usize));
    u.div(&BigInt::from_decimal(&divisor_dec))
}

fn native_bd_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // JDK narrowing conversion: toBigInteger().intValue() — the low 32
    // two's-complement bits of the truncated value. The old f64 path both
    // saturated (f64→i32 casts clamp) and lost precision past 2^53.
    let t = bd_truncated_bigint(ctx, this);
    Ok(Some(Value::Int(
        bigint_low_twos_complement(&t, 32) as u32 as i32
    )))
}

fn native_bd_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // JDK narrowing conversion: low 64 two's-complement bits — see intValue.
    let t = bd_truncated_bigint(ctx, this);
    Ok(Some(Value::Long(bigint_low_twos_complement(&t, 64) as i64)))
}

/// `BigDecimal.toBigInteger()` — truncate the fraction (setScale(0, DOWN))
/// and return the integer part. Registered as a real-JDK override because the
/// real bytecode path (`setScale(0,1)` → `divideAndRound` → MutableBigInteger
/// long division, all interpreted) dominates Lucene `TestUtil.nextLong`'s
/// large-range branch (ES codec/doc-values test timeouts).
fn native_bd_to_big_integer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let t = bd_truncated_bigint(ctx, this);
    let obj = bi_alloc_int(ctx, &t);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_bd_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let v: f64 = bd_read(ctx, this).parse().unwrap_or(0.0);
    Ok(Some(Value::Double(v)))
}

fn native_bd_float_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let v: f64 = bd_read(ctx, this).parse().unwrap_or(0.0);
    Ok(Some(Value::Float(v as f32)))
}

fn native_bd_scale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(bd_scale_of(ctx, this))))
}

fn native_bd_precision(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let p = bd_precision_of(ctx, this);
    if p > 0 {
        return Ok(Some(Value::Int(p)));
    }
    // The slot's 0 is the JDK's lazy "not yet computed" sentinel
    // (valueOf/real-bytecode arithmetic leave it 0). This native shadows
    // `precision()` INSIDE the real `compareTo`'s compareMagnitude
    // (adjusted-exponent quick exit); returning the raw 0 degrades that
    // comparison to compare-by-scale, which is order-inconsistent across
    // mixed-provenance values (the H2 GROUP-BY TreeMap pseudo-hang).
    // Compute the true significant-digit count and cache it, like the JDK.
    let (u, _s) = bd_unscaled_bigint(ctx, this);
    let dec = u.to_decimal();
    let abs = dec.strip_prefix('-').unwrap_or(&dec);
    let trimmed = abs.trim_start_matches('0');
    let computed = if trimmed.is_empty() {
        1
    } else {
        trimmed.len() as i32
    };
    let idx = bd_layout(ctx)
        .map(|(_, _, pr, _)| pr)
        .unwrap_or(BD_FIELD_PRECISION);
    ctx.set_field(this, idx, Value::Int(computed));
    Ok(Some(Value::Int(computed)))
}

fn native_bd_negate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Sign-flip on the exact unscaled value (the rendered string bakes
    // negative-scale trailing zeros in — see `bd_unscaled_and_precision`).
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    let dec = u.to_decimal();
    let neg = if let Some(stripped) = dec.strip_prefix('-') {
        stripped.to_string()
    } else if dec == "0" {
        dec
    } else {
        format!("-{}", dec)
    };
    let result = bd_alloc_bigint(ctx, &crate::bigint::BigInt::from_decimal(&neg), scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    let dec = u.to_decimal();
    let abs = dec.strip_prefix('-').unwrap_or(&dec).to_string();
    let result = bd_alloc_bigint(ctx, &crate::bigint::BigInt::from_decimal(&abs), scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_signum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Exact sign from the unscaled value — this native shadows `signum()`
    // inside the real `compareTo` bytecode, so an f64 parse (the old
    // implementation; underflows past ~1e-324) must not decide ordering.
    let (u, _scale) = bd_unscaled_bigint(ctx, this);
    let dec = u.to_decimal();
    Ok(Some(Value::Int(if dec == "0" {
        0
    } else if dec.starts_with('-') {
        -1
    } else {
        1
    })))
}

/// Decide whether `|quotient|` must be incremented (rounded away from zero)
/// given the truncated `(quotient, remainder)` of `unscaled / 10^drop` and
/// the requested rounding mode. `dividend_neg` is the sign of the original
/// unscaled value (== the sign of a nonzero `remainder`).
///
/// Mirrors `java.math.BigDecimal`'s rounding semantics exactly, operating on
/// the binary `BigInt` remainder/divisor directly (a `shl(1)` doubling and a
/// `cmp`) instead of ever formatting through `f64` — the old
/// `bd_read(...).parse::<f64>()` implementation both truncated precision for
/// any unscaled value wider than ~17 significant digits AND was the
/// dominant cost (~580us/call, measured) behind `TestUtil.nextLong`'s
/// large-range path timing out Lucene's postings/doc-values randomized
/// tests (`BigDecimal(double).toBigInteger()` calls `setScale(0, DOWN)`
/// millions of times per test class).
fn bd_round_needs_increment(
    remainder: &crate::bigint::BigInt,
    divisor: &crate::bigint::BigInt,
    quotient: &crate::bigint::BigInt,
    dividend_neg: bool,
    mode: i32,
) -> Result<bool, ()> {
    if remainder.is_zero() {
        return Ok(false);
    }
    Ok(match mode {
        BD_ROUND_DOWN => false,
        BD_ROUND_UP => true,
        BD_ROUND_CEILING => !dividend_neg,
        BD_ROUND_FLOOR => dividend_neg,
        BD_ROUND_HALF_UP | BD_ROUND_HALF_DOWN | BD_ROUND_HALF_EVEN => {
            let abs_rem = if remainder.is_neg() {
                remainder.neg_value()
            } else {
                remainder.clone()
            };
            let twice = abs_rem.shl(1);
            match twice.cmp(divisor) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => match mode {
                    BD_ROUND_HALF_UP => true,
                    BD_ROUND_HALF_DOWN => false,
                    // HALF_EVEN: increment only if that makes the kept
                    // digit even, i.e. the truncated quotient is odd.
                    _ => quotient.test_bit(0),
                },
            }
        }
        BD_ROUND_UNNECESSARY => return Err(()),
        // Unknown mode — HotSpot's own RoundingMode enum bounds this to
        // 0..=7; be conservative and don't round rather than guess.
        _ => false,
    })
}

/// Shared implementation for `setScale(int)` / `setScale(int,int)` /
/// `setScale(int,RoundingMode)` (the last two delegate to `(II)` in real
/// bytecode). Rescales the *exact* unscaled `BigInt` — never a lossy `f64`
/// round-trip — so both correctness (values with >17 significant digits)
/// and performance (the old path's slow-path `f64::parse` on long decimal
/// strings) are fixed together.
fn bd_set_scale_impl(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    new_scale: i32,
    mode: i32,
) -> MethodCallResult {
    use crate::bigint::BigInt;
    let (unscaled, scale) = bd_unscaled_bigint(ctx, this);
    if new_scale >= scale {
        let padded = bigint_mul_pow10(&unscaled, new_scale - scale);
        let result = bd_alloc_bigint(ctx, &padded, new_scale);
        return Ok(Some(Value::Object(Some(result))));
    }
    let drop = (scale - new_scale) as usize;
    let mut divisor_dec = String::with_capacity(drop + 1);
    divisor_dec.push('1');
    divisor_dec.push_str(&"0".repeat(drop));
    let divisor = BigInt::from_decimal(&divisor_dec);
    let (quotient, remainder) = unscaled.divmod(&divisor);
    let dividend_neg = unscaled.is_neg();
    let increment =
        match bd_round_needs_increment(&remainder, &divisor, &quotient, dividend_neg, mode) {
            Ok(v) => v,
            Err(()) => {
                return Err(RuntimeError::ArithmeticException {
                    message: "Rounding necessary".to_string(),
                }
                .into());
            }
        };
    let rounded = if increment {
        let one = BigInt::from_decimal(if dividend_neg { "-1" } else { "1" });
        quotient.add(&one)
    } else {
        quotient
    };
    let result = bd_alloc_bigint(ctx, &rounded, new_scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_set_scale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_scale = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // `setScale(int)` == `setScale(newScale, ROUND_UNNECESSARY)` per spec.
    bd_set_scale_impl(ctx, this, new_scale, BD_ROUND_UNNECESSARY)
}

fn native_bd_set_scale_rounding(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_scale = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let mode = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => BD_ROUND_UNNECESSARY,
    };
    bd_set_scale_impl(ctx, this, new_scale, mode)
}

fn native_bd_strip_zeros(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let a = bd_read(ctx, this);
    let stripped = if a.contains('.') {
        a.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        a
    };
    let scale = stripped
        .find('.')
        .map(|p| (stripped.len() - p - 1) as i32)
        .unwrap_or(0);
    let result = bd_alloc(ctx, &stripped, scale);
    Ok(Some(Value::Object(Some(result))))
}

fn native_bd_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = bd_read(ctx, this);
    let mut h: i32 = 0;
    for b in a.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i32);
    }
    Ok(Some(Value::Int(h)))
}

/// `BigInteger.toString(int)` conformance.
///
/// Every expectation here was read off real JDK 25.0.3 (`java Oracle.java`),
/// not derived from this implementation.
#[cfg(test)]
mod bi_to_string_radix_tests {
    use super::bi_to_radix_string;
    use crate::bigint::BigInt;
    use crate::java_radix_or_ten;

    fn s(dec: &str, radix: u32) -> String {
        bi_to_radix_string(&BigInt::from_decimal(dec), radix)
    }

    /// The regression this file's `toString(int)` actually had: negatives were
    /// rendered with `format!("{:x}", i128)`, i.e. the two's-complement bit
    /// pattern. The JDK renders SIGN-MAGNITUDE.
    #[test]
    fn negatives_are_sign_magnitude_never_twos_complement() {
        assert_eq!(s("-1", 16), "-1");
        assert_eq!(s("-1", 2), "-1");
        assert_eq!(s("-1", 8), "-1");
        assert_eq!(s("-255", 16), "-ff");
        assert_eq!(s("-5", 2), "-101");
        assert_eq!(s("-255", 36), "-73");
        // The exact shape the old body produced, spelled out so a revert is
        // unmistakable rather than merely "not equal".
        assert_ne!(s("-1", 16), "ffffffffffffffffffffffffffffffff");
        assert_ne!(s("-1", 16), "ffffffff");
    }

    /// Radices other than 2/8/16 used to fall through to the DECIMAL string.
    #[test]
    fn every_legal_radix_is_honoured_not_just_two_eight_sixteen() {
        assert_eq!(s("255", 3), "100110");
        assert_eq!(s("255", 36), "73");
        assert_eq!(s("255", 16), "ff");
        assert_eq!(s("255", 8), "377");
        assert_eq!(s("255", 2), "11111111");
        assert_eq!(s("255", 10), "255");
        // Both boundary radices.
        assert_eq!(s("5", 2), "101");
        assert_eq!(s("5", 36), "5");
    }

    /// The old body narrowed through `i128` with `.unwrap_or(0)`, so anything
    /// past `i128::MAX` silently answered "0".
    #[test]
    fn values_past_i128_keep_every_digit() {
        // 2^200 + 7
        let huge = "1606938044258990275541962092341162602522202993782792835301383";
        assert_eq!(
            s(huge, 16),
            "100000000000000000000000000000000000000000000000007"
        );
        assert_eq!(s(huge, 2).len(), 201);
        assert_eq!(s(huge, 10), huge);
        let big = "123456789012345678901234567890";
        assert_eq!(s(big, 16), "18ee90ff6c373e0ee4e3f0ad2");
        assert_eq!(s(big, 36), "byw97um9s91dlz68tsi");
        assert_eq!(s(big, 8), "143564417755415637016711617605322");
        assert_eq!(
            s(&format!("-{big}"), 16),
            "-18ee90ff6c373e0ee4e3f0ad2"
        );
        assert_eq!(s(&format!("-{big}"), 36), "-byw97um9s91dlz68tsi");
    }

    /// `Long.MIN_VALUE` has no positive counterpart — the classic overflow
    /// trap. As a `BigInteger` it is just another magnitude, so assert it.
    #[test]
    fn long_min_value_and_zero() {
        assert_eq!(s("-9223372036854775808", 16), "-8000000000000000");
        assert_eq!(s("-9223372036854775808", 36), "-1y2p0ij32e8e8");
        assert_eq!(
            s("-9223372036854775808", 2),
            "-1000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(s("9223372036854775807", 36), "1y2p0ij32e8e7");
        for r in 2..=36u32 {
            assert_eq!(s("0", r), "0", "zero in radix {r}");
        }
    }

    /// Interior chunks must be zero-padded to the full chunk width; only the
    /// most significant chunk may be short. A padding bug shows up as digits
    /// going missing in the middle of a long rendering.
    #[test]
    fn chunk_padding_round_trips_through_decimal() {
        for r in 2..=36u32 {
            for dec in [
                "1606938044258990275541962092341162602522202993782792835301383",
                "123456789012345678901234567890",
                "4294967296",
                "18446744073709551616",
                "-18446744073709551616",
            ] {
                let rendered = bi_to_radix_string(&BigInt::from_decimal(dec), r);
                let (neg, abs) = match rendered.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, rendered.as_str()),
                };
                let mut acc = BigInt::zero();
                let base = BigInt::from_le_words(false, vec![r]);
                for ch in abs.chars() {
                    let d = ch.to_digit(r).expect("digit in range");
                    acc = acc.mul(&base).add(&BigInt::from_le_words(false, vec![d]));
                }
                if neg {
                    acc = acc.neg_value();
                }
                assert_eq!(acc.to_decimal(), dec, "radix {r} round-trip of {dec}");
            }
        }
    }

    /// `toString(int)` IGNORES an out-of-range radix and uses 10 — it does not
    /// throw and it does not clamp to 2/36. Measured on real JDK 25.
    #[test]
    fn out_of_range_radix_substitutes_ten() {
        for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
            assert_eq!(java_radix_or_ten(bad), 10, "radix {bad}");
            assert_eq!(s("255", java_radix_or_ten(bad)), "255", "radix {bad}");
            assert_eq!(s("-255", java_radix_or_ten(bad)), "-255", "radix {bad}");
        }
        assert_eq!(java_radix_or_ten(2), 2);
        assert_eq!(java_radix_or_ten(36), 36);
    }
}
