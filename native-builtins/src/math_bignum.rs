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

/// Read a `BigInteger`'s `mag:[I` payload in ONE bulk copy, big-endian order
/// preserved.
///
/// PERF (2026-08-18, commons-math `LegendreHighPrecisionTest`): every read
/// boundary here used to walk the array with `ctx.get_array_element(mag, i)`.
/// That is a trait-object dispatch, a `Value` box, AND — on ZGC — a full
/// `audit_access_receiver` address validation (`VmHeap::is_object_address` +
/// `ZObjectStarts::contains`) **per word**. `perf record` on a 3M-iteration
/// `BigInteger.multiply` loop attributed ~30% of the whole process to that
/// per-element plumbing, against 3.6% for `BigInt::mul` — the arithmetic
/// itself. `read_int_array_into` is one bounds check and one
/// `copy_nonoverlapping`; the words are validated once, not `len` times.
///
/// The per-element loop is kept as a fallback for the cases the bulk path
/// declines (wrong array kind, and G1's humongous `int[]`, which has no flat
/// `array_data_ptr`) so behaviour is unchanged wherever the memcpy is not
/// available.
fn bi_read_mag_be(ctx: &dyn NativeContext, mag: ObjectRef, len: usize) -> Vec<u32> {
    let mut buf = vec![0i32; len];
    if len == 0 || ctx.read_int_array_into(mag, 0, &mut buf) == len {
        // `u32` and `i32` have the same bit pattern; this is the same
        // `v as u32` the element loop applied.
        return buf.into_iter().map(|v| v as u32).collect();
    }
    let mut words: Vec<u32> = Vec::with_capacity(len);
    for i in 0..len {
        let w = match ctx.get_array_element(mag, i) {
            Value::Int(v) => v as u32,
            _ => 0,
        };
        words.push(w);
    }
    words
}

/// Write a big-endian magnitude into a freshly allocated `mag:[I` in ONE bulk
/// copy — the write twin of [`bi_read_mag_be`], same rationale.
///
/// The array is freshly allocated by the caller and not yet reachable from any
/// Java root, so there is no store barrier to run per element (`int[]` elements
/// are primitives — no reference stores at all).
fn bi_write_mag_be(ctx: &mut dyn NativeContext, mag: ObjectRef, be: &[u32]) {
    // SAFETY of the cast: `[u32]` and `[i32]` have identical layout; the
    // element loop below performs the same `w as i32` reinterpretation.
    let signed: &[i32] = unsafe { std::slice::from_raw_parts(be.as_ptr() as *const i32, be.len()) };
    if ctx.write_int_array_from(mag, 0, signed) {
        return;
    }
    for (i, &w) in be.iter().enumerate() {
        ctx.set_array_element(mag, i, Value::Int(w as i32));
    }
}

/// RBIGDEC.1 — Resolve the real-JDK BigInteger field layout if available.
///
/// Returns `Some((signum_idx, mag_idx))` when the JDK class is loaded with the
/// real fields `signum:I` and `mag:[I`.  Returns `None` in synthetic-jdk mode
/// or before the class has been loaded — callers fall back to the legacy
/// 2-field synthetic layout (`BI_FIELD_VALUE` / `BI_FIELD_SIGNUM`).
pub(crate) fn bi_layout(ctx: &dyn NativeContext) -> Option<(usize, usize)> {
    if let Some(cached) = bi_layout_cached(ctx.vm_identity()) {
        return Some(cached);
    }
    let s = ctx.resolve_field_index("java/math/BigInteger", "signum")?;
    let m = ctx.resolve_field_index("java/math/BigInteger", "mag")?;
    bi_layout_store(ctx.vm_identity(), (s, m));
    Some((s, m))
}

// PERF (2026-08-18, commons-math `LegendreHighPrecisionTest`): `bi_layout` was
// resolving BOTH field indices BY NAME on every single BigInteger/BigDecimal
// native call, and every one of those `resolve_field_index` calls takes the
// class-manager `RwLock`, hashes `"java/math/BigInteger"`, `memcmp`s it against
// the loaded-class table, then walks the field list comparing names. A single
// `BigDecimal.multiply(mc)` reaches it six-plus times (two operand reads, the
// result allocation, `precision()`, …). `perf record` on a 3M-iteration
// `BigInteger.multiply` loop put `resolve_field_index` + `get_loaded_class_id` +
// the `loaded_classes_probe` hash search + its `memcmp` at ~12% of the whole
// process — more than `BigInt::mul`, the actual arithmetic, at 3.6%.
//
// The layout of `java.math.BigInteger` / `java.math.BigDecimal` is fixed for
// the life of a VM once the class is loaded, so it is resolved once and then
// read from a thread-local. Two properties keep the cache honest:
//
//   * It is scoped by `vm_identity()` — Rust tests build several independent
//     `Vm`s in one process, and a synthetic-JDK VM has NO such layout at all.
//     A cache entry from another VM is never returned.
//   * Only a **successful** resolve is stored. Before `java.math.BigInteger`
//     is loaded the resolve legitimately answers `None`, and caching that would
//     pin every later call to the synthetic-stub fallback.
//
// Thread-local rather than a shared atomic cell: this is on the per-call path
// of every bignum native, and a `static` would put an atomic read (and, with
// several mutator threads, cache-line ping-pong) where there is now none.
thread_local! {
    static BI_LAYOUT_TLS: std::cell::Cell<Option<(usize, (usize, usize))>> =
        const { std::cell::Cell::new(None) };
    static BD_LAYOUT_TLS: std::cell::Cell<Option<(usize, (usize, usize, usize, usize))>> =
        const { std::cell::Cell::new(None) };
    // `BigDecimal.stringCache`'s slot, same scoping (`bd_string_cache_index`).
    static BD_STRING_CACHE_TLS: std::cell::Cell<Option<(usize, usize)>> =
        const { std::cell::Cell::new(None) };
    // `MathContext`/`RoundingMode` slots (`bd_mc_layout`).
    static MC_LAYOUT_TLS: std::cell::Cell<Option<(usize, (usize, usize, Option<usize>))>> =
        const { std::cell::Cell::new(None) };
}

fn bi_layout_cached(vm: usize) -> Option<(usize, usize)> {
    BI_LAYOUT_TLS.with(|c| match c.get() {
        Some((owner, layout)) if owner == vm => Some(layout),
        _ => None,
    })
}

fn bi_layout_store(vm: usize, layout: (usize, usize)) {
    BI_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
}

fn bd_layout_cached(vm: usize) -> Option<(usize, usize, usize, usize)> {
    BD_LAYOUT_TLS.with(|c| match c.get() {
        Some((owner, layout)) if owner == vm => Some(layout),
        _ => None,
    })
}

fn bd_layout_store(vm: usize, layout: (usize, usize, usize, usize)) {
    BD_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
}

// PERF (2026-08-18, same profile as the layout memo above): every
// `BigInteger`/`BigDecimal` result object is allocated through
// `try_alloc_concurrent_synthetic`, which re-resolves the class BY NAME on
// each call — `ensure_class_initialized("java/math/BigInteger")` walks the
// class manager's loaded-class table (`load_class_concurrent_for` +
// the `loaded_classes_probe` hash search), then `class_name_of_id` renders the
// resolved id back to a string to compare it against the name we just passed
// in. On the `BigInteger.multiply` loop that was ~6% of the process, once per
// allocated result.
//
// Once the REAL JDK class is loaded its `ClassId` is fixed for the life of the
// VM, so it is memoized per VM (same `vm_identity()` scoping and same
// only-cache-a-success rule as the layout memo). Two deliberate narrowings
// keep the memo away from everything that is not that case:
//
//   * It is armed only when the caller has already seen the real-JDK layout
//     (`bi_layout`/`bd_layout` answered `Some`). In synthetic-JDK mode the
//     allocation funnel may FABRICATE a class for the name, and a fabricated
//     class's identity and field count are not stable across calls — those
//     runs keep going through the funnel unchanged.
//   * The slot count is still derived fresh from `class_num_total_fields(cid)`
//     on every allocation, exactly as the funnel does it, so nothing about the
//     sizing decision moves — only the name→id lookup is skipped.
thread_local! {
    static BIGNUM_CID_TLS: std::cell::Cell<Option<(usize, ClassId, ClassId)>> =
        const { std::cell::Cell::new(None) };
}

fn bignum_cids(vm: usize) -> Option<(ClassId, ClassId)> {
    BIGNUM_CID_TLS.with(|c| match c.get() {
        Some((owner, bi, bd)) if owner == vm => Some((bi, bd)),
        _ => None,
    })
}

fn bignum_cids_store(vm: usize, bi: ClassId, bd: ClassId) {
    BIGNUM_CID_TLS.with(|c| c.set(Some((vm, bi, bd))));
}

/// Which of the two bignum classes a memoized allocation is for.
#[derive(Clone, Copy)]
enum BignumClass {
    Integer,
    Decimal,
}

/// Allocate a `java.math.BigInteger` / `java.math.BigDecimal` instance,
/// skipping the by-name class resolution once it is known for this VM.
///
/// Falls back to `try_alloc_concurrent_synthetic` verbatim whenever the memo is
/// not armed, so the synthetic-JDK and not-yet-loaded paths are untouched.
fn bignum_alloc(
    ctx: &mut dyn NativeContext,
    which: BignumClass,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_name = match which {
        BignumClass::Integer => "java/math/BigInteger",
        BignumClass::Decimal => "java/math/BigDecimal",
    };
    let vm = ctx.vm_identity();
    if let Some((bi_cid, bd_cid)) = bignum_cids(vm) {
        let cid = match which {
            BignumClass::Integer => bi_cid,
            BignumClass::Decimal => bd_cid,
        };
        let n = num_fields.max(ctx.class_num_total_fields(cid));
        return Ok(ctx
            .try_alloc_object_gc_safe(cid, n)
            .unwrap_or_else(|| ctx.alloc_object(cid, n)));
    }
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, num_fields)?;
    // Arm the memo only when BOTH real-JDK classes are loaded with the layout
    // these natives read — that is the one state in which the ids are fixed.
    if bi_layout(ctx).is_some() && bd_layout(ctx).is_some() {
        if let (Some(bi_cid), Some(bd_cid)) = (
            ctx.class_id_by_name("java/math/BigInteger"),
            ctx.class_id_by_name("java/math/BigDecimal"),
        ) {
            bignum_cids_store(vm, bi_cid, bd_cid);
        }
    }
    Ok(obj)
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
        let words = bi_read_mag_be(ctx, mag, len);
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

pub(crate) fn bi_alloc(
    ctx: &mut dyn NativeContext,
    value: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = bignum_alloc(ctx, BignumClass::Integer, 2)?;
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
        bi_write_mag_be(ctx, mag_arr, &mag_words);
        ctx.set_field(obj, sig_i, Value::Int(signum));
        ctx.set_field(obj, mag_i, Value::Object(Some(mag_arr)));
        ctx.unpin_native_roots(h);
        Ok(obj)
    } else {
        // Synthetic-stub fallback. The value string is private to this object:
        // uninterned (round 13 wave 9), or every value ever built stayed in the
        // VM's never-pruned string pool.
        let s = ctx.create_string_uninterned(value);
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BI_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BI_FIELD_SIGNUM, Value::Int(signum));
        ctx.unpin_native_roots(h);
        Ok(obj)
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
        let mut words = bi_read_mag_be(ctx, mag, len);
        words.reverse();
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
pub(crate) fn bi_alloc_int(
    ctx: &mut dyn NativeContext,
    v: &crate::bigint::BigInt,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = bignum_alloc(ctx, BignumClass::Integer, 2)?;
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
        let be: Vec<u32> = le.iter().rev().copied().collect();
        bi_write_mag_be(ctx, mag_arr, &be);
        ctx.set_field(obj, sig_i, Value::Int(signum));
        ctx.set_field(obj, mag_i, Value::Object(Some(mag_arr)));
        ctx.unpin_native_roots(h);
        Ok(obj)
    } else {
        // Uninterned, like `bi_alloc`'s synthetic arm.
        let s = ctx.create_string_uninterned(&v.to_decimal());
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BI_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BI_FIELD_SIGNUM, Value::Int(signum));
        ctx.unpin_native_roots(h);
        Ok(obj)
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
        // NOT `-n`: that OVERFLOWS for `i32::MIN` — a panic in a debug build,
        // and in release it wraps straight back to `i32::MIN`, so these two
        // helpers called each other forever (stack overflow). `unsigned_abs` is
        // the JDK's own rule, "-n considered unsigned"
        // (BigInteger.java:3502-3504), and a right shift of 2^31 or more bits
        // clears every magnitude word: 0, or -1 for a negative value.
        let k = n.unsigned_abs();
        if k > i32::MAX as u32 {
            return if value.starts_with('-') {
                "-1".to_string()
            } else {
                "0".to_string()
            };
        }
        return bi_shift_right_str(value, k as i32);
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
        // See `bi_shift_left_str`: `-n` overflows for `i32::MIN` and the two
        // helpers then recurse into each other forever. A LEFT shift of 2^31 or
        // more bits is `ArithmeticException("BigInteger would overflow
        // supported range")` on HotSpot; this `String`-returning helper has no
        // error channel and is NOT registered as a native (the registered
        // `shiftLeft`/`shiftRight` enforce it via `p71_bi_checked_shl`), so the arm
        // is unreachable from Java — it must simply not recurse.
        let k = n.unsigned_abs();
        if k > i32::MAX as u32 {
            return value.to_string();
        }
        return bi_shift_left_str(value, k as i32);
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
        if neg && q == "1" {
            // ARGUMENT-DRIVEN LOOP (fixed 2026-08-13, lane F7). The `q == "0"`
            // guard above only ever fires for a NON-NEGATIVE value: the
            // negative arm computes `ceildiv(q, 2)`, whose fixpoint is 1, not
            // 0. So `bi_shift_right_str("-1", Integer.MAX_VALUE)` ran the full
            // 2^31 iterations — a decimal division each — to return "-1",
            // which the sign-extension arm below already knows. The loop count
            // came from the ARGUMENT while the answer had stopped changing.
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

/// Whether a decimal-string helper below takes its limb road for these
/// unsigned operands: one of them is wider than 18 digits, every one is
/// canonical (`"0"`, or ASCII digits with no leading zero: the `bi_read` form
/// the decimal loops assume, so both roads see the same values), and
/// `CRATONVM_BIGINT_STR_HELPERS_LIMB` is not `0` (round 14 wave 2, lane
/// bigdec). The switch is read only for wide operands.
fn bi_str_helper_takes_limbs(operands: &[&str]) -> bool {
    operands.iter().any(|s| s.len() > 18)
        && operands.iter().all(|s| {
            *s == "0"
                || (!s.is_empty() && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit()))
        })
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGINT_STR_HELPERS_LIMB")
}

/// `BigInteger.gcd(other)` — Euclidean GCD on absolute values.
pub(crate) fn bi_gcd_str(a: &str, b: &str) -> String {
    let (_, a_abs) = bi_parse_sign(a);
    let (_, b_abs) = bi_parse_sign(b);
    // Round 14 wave 2 (lane bigdec): past 18 digits, Euclid over limbs (one
    // Knuth division per step) instead of decimal long division, which made
    // each step O(digits^2) and the whole gcd O(digits^3). Same value, same
    // canonical rendering. `CRATONVM_BIGINT_STR_HELPERS_LIMB=0` keeps the
    // decimal loop below.
    if bi_str_helper_takes_limbs(&[a_abs, b_abs]) {
        let mut x = crate::bigint::BigInt::from_decimal(a_abs);
        let mut y = crate::bigint::BigInt::from_decimal(b_abs);
        while !y.is_zero() {
            let r = x.rem(&y);
            x = y;
            y = r;
        }
        return x.to_decimal();
    }
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
/// signed decimal strings.
///
/// TOTAL since 2026-08-16 (lane G10, closing F31-1 N3). This used to end a
/// negative exponent with
///
/// ```text
///     panic!("bi_mod_pow_str: negative exponent — caller must compute modInverse first");
/// ```
///
/// whose own comment said it chose to "panic to be loud". A Rust `panic!` in a
/// native is not a Java throwable: it unwinds past every `catch` and takes the
/// VM with it, which is the one failure mode no Java program can survive. A
/// negative exponent is *legal* `BigInteger` — MEASURED on
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Temurin),
/// `scratchpad/g10/BiProbe.java`:
///
/// ```text
/// 3.modPow(-1, 7)  = 5        (-3).modPow(3, 7) = 1        3.modPow(2, 1) = 0
/// 2.modPow(-1, 4) !! ArithmeticException: BigInteger not invertible.
/// 3.modPow(2, 0)  !! ArithmeticException: BigInteger: modulus not positive
/// 3.modPow(2, -7) !! ArithmeticException: BigInteger: modulus not positive
/// ```
///
/// So there is exactly one shape a `-> String` signature cannot express: a
/// negative exponent over a non-invertible base. [`bi_mod_pow_str_opt`] is the
/// real body and returns `None` for it; this wrapper keeps the infallible
/// signature its out-of-lane caller (`phases_late::register_p71_biginteger_extras`)
/// needs and answers `"0"` there.
///
/// **`"0"` is a wrong value, and it is deliberately preferred to a VM abort.**
/// Priority order for this family is: a VM abort is worse than a wrong value.
/// No in-tree caller can reach it — all four (`bi_miller_rabin_str`, this file's
/// `modPow` native, `phases_late:8847`, and `bigint.rs`'s differential tests)
/// strip the sign or invert the base before calling. Callers that can see a
/// caller-chosen exponent should take [`bi_mod_pow_str_opt`] and raise
/// `ArithmeticException("BigInteger not invertible.")` on `None`; that is this
/// lane's NOMINATION for `phases_late.rs`.
pub(crate) fn bi_mod_pow_str(base: &str, exp: &str, m: &str) -> String {
    bi_mod_pow_str_opt(base, exp, m).unwrap_or_else(|| "0".to_string())
}

/// [`bi_mod_pow_str`] with the one case a `String` cannot carry: `None` means
/// "negative exponent over a base with no inverse mod `m`", i.e. HotSpot's
/// `ArithmeticException("BigInteger not invertible.")`.
pub(crate) fn bi_mod_pow_str_opt(base: &str, exp: &str, m: &str) -> Option<String> {
    if m == "1" || m == "-1" {
        return Some("0".to_string());
    }
    // LIMBS, not decimal digits.
    //
    // This used to be square-and-multiply over decimal STRINGS: one
    // `bi_mul_unsigned` (schoolbook decimal) plus one `bi_mod_unsigned`
    // (decimal long division) per exponent bit. For RSA-2048 that is ~2400
    // modular operations on 617-digit numbers, and it is what
    // `biginteger-limb-rewrite-scope.md` identified in May as the ~100x
    // constant-factor loss blocking every crypto-heavy suite.
    //
    // `BigInt` (step 1 of that rewrite) has carried a Montgomery `modpow` and
    // a differential test against this very function since it landed; nothing
    // routed through it. This is that wiring for the one operation the crypto
    // suites actually sit on -- `oddModPow` is what every RSA/DSA/DH
    // private-key operation and every Miller-Rabin round runs.
    //
    // Measured on netty's `testMutualAuthSameCertChain`, which builds 96
    // self-signed certificates: see the branch's page.
    let (_, m_abs) = bi_parse_sign(m);
    let (e_neg, e_abs) = bi_parse_sign(exp);
    let modulus = crate::bigint::BigInt::from_decimal(m_abs);
    if modulus.is_zero() {
        return Some("0".to_string());
    }
    // A negative exponent is `this.modInverse(m).modPow(-exp, m)`, the JDK's
    // own rule (`BigInteger.java`), kept here rather than pushed into
    // `BigInt::modpow` -- which takes the exponent's MAGNITUDE and so cannot
    // see the sign.
    let mut b = crate::bigint::BigInt::from_decimal(base).modulo(&modulus);
    if e_neg {
        b = b.mod_inverse(&modulus)?;
    }
    let e = crate::bigint::BigInt::from_decimal(e_abs);
    Some(b.modpow(&e, &modulus).to_decimal())
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
    use super::{bi_alloc_int, bi_mod_inverse_str, bi_mod_pow_str};
    use crate::bigint::BigInt;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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

/// `BigInteger.toByteArray()` of a limb value, by its definition
/// (`BigInteger.java` `toByteArray`): `bitLength() / 8 + 1` bytes of the
/// two's-complement value, big-endian (round 14 wave 2, lane bigdec).
pub(crate) fn bi_to_byte_array_int(v: &crate::bigint::BigInt) -> Vec<u8> {
    let len = (v.bit_length() / 8 + 1) as usize;
    let mut le: Vec<u8> = Vec::with_capacity(len + 4);
    for w in v.mag_le() {
        le.extend_from_slice(&w.to_le_bytes());
    }
    // `len` bytes hold the magnitude (its top bit is below `8 * len`, and a
    // negative power of two needs one bit less), so this only pads or drops
    // zero bytes.
    le.resize(len, 0);
    if v.is_neg() {
        // Two's complement over `len` bytes: invert, then add one.
        let mut carry = true;
        for b in le.iter_mut() {
            let (sum, overflow) = (!*b).overflowing_add(u8::from(carry));
            *b = sum;
            carry = overflow;
        }
    }
    le.reverse();
    le
}

/// `BigInteger.toByteArray()` — two's-complement big-endian byte encoding,
/// with the minimal length needed to represent the value (always at least
/// one byte). Sign-extends.
pub(crate) fn bi_to_byte_array_str(value: &str) -> Vec<u8> {
    if value == "0" {
        return vec![0u8];
    }
    let (neg, abs) = bi_parse_sign(value);
    // Round 14 wave 2 (lane bigdec): past 18 digits, the bytes straight from
    // the limbs (one parse) instead of one decimal long division by 256 per
    // BYTE below. `CRATONVM_BIGINT_STR_HELPERS_LIMB=0` keeps the loop.
    if bi_str_helper_takes_limbs(&[abs]) {
        return bi_to_byte_array_int(&crate::bigint::BigInt::from_decimal(value));
    }
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
    // `add`, `subtract` and `multiply` are NOT registered here -- deleted
    // 2026-09-12, and the reason is the same shape as the `and`/`or`/`xor`/
    // `not`/`bitLength`/`bitCount`/`testBit`/`toByteArray` note below: a limb
    // twin is already registered, LATER, by
    // `phases_late::register_p71_biginteger_extras`, so these three never owned
    // the slot in any of the three feature arms. MEASURED via
    // `NativeMethodRegistry::census()` in all three:
    //
    //   add/subtract/multiply  Intrinsic here  dead    Bridge in phases_late  OWNER
    //
    // Dead is not harmless. `--jdk-only` refuses the retired `Bridge`, and a
    // refusal declines to insert THAT registration rather than emptying the
    // slot -- so with these three present the strict registry fell back to
    // them, and they answer **null** for a null argument where the real body
    // throws `NullPointerException`. Lane 2 could not retire the three rows for
    // exactly this reason, and the wave still MEASURED as accepted because the
    // probe rows moved. `stub_ratchet.rs`'s
    // `no_retired_triple_survives_the_strict_boot` is the gate that now makes
    // the arrangement impossible to ship again.
    //
    // The bodies stay: `register_biginteger_natives` below still registers all
    // three for synthetic-jdk mode, where no real bytecode exists to fall to.
    // Round 13 wave 9 (lane bigdec2): the JDK shares `mag` with the negation.
    // `CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG=0` registers the copying body.
    let shares =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG");
    let bi_negate_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if shares {
        native_bi_negate_shared
    } else {
        native_bi_negate
    };
    registry.register(bi, "negate", "()Ljava/math/BigInteger;", bi_negate_body);
    registry.register(bi, "signum", "()I", native_bi_signum);
    // Round 13 wave 8 (lane bigdec): an uninterned result, like HotSpot's.
    // `CRATONVM_BIGNUM_STRINGS_UNINTERNED=0` registers the interning body.
    let fresh =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGNUM_STRINGS_UNINTERNED");
    let bi_to_string_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if fresh {
        native_bi_to_string_uninterned
    } else {
        native_bi_to_string
    };
    registry.register(bi, "toString", "()Ljava/lang/String;", bi_to_string_body);
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
    // Round 13 wave 9 (lane bigdec2): count digits without rendering them.
    // `CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER=0` registers the old body.
    let counted =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER");
    let bd_precision_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if counted {
        native_bd_precision_counted
    } else {
        native_bd_precision
    };
    registry.register(bd, "precision", "()I", bd_precision_body);
    // Same wave: one compact object, as the JDK builds it.
    // `CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT=0` registers the old body.
    let compact =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT");
    let bd_value_of_long_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        if compact {
            native_bd_value_of_long_compact
        } else {
            native_bd_value_of_long
        };
    registry.register(
        bd,
        "valueOf",
        "(J)Ljava/math/BigDecimal;",
        bd_value_of_long_body,
    );
    registry.register(
        bd,
        "valueOf",
        "(D)Ljava/math/BigDecimal;",
        native_bd_value_of_double,
    );
    // Round 13 wave 8 (lane bigdec): the JDK's `stringCache`, uninterned.
    // `CRATONVM_BIGDECIMAL_TOSTRING_CACHE=0` registers the old body.
    let cached =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_TOSTRING_CACHE");
    let bd_to_string_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if cached {
        native_bd_to_string_cached
    } else {
        native_bd_to_string
    };
    registry.register(bd, "toString", "()Ljava/lang/String;", bd_to_string_body);
    // Same wave: uninterned (`native_bi_to_string_uninterned` has the why).
    let fresh =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGNUM_STRINGS_UNINTERNED");
    let bd_plain_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if fresh {
        native_bd_to_plain_string_uninterned
    } else {
        native_bd_to_plain_string
    };
    registry.register(bd, "toPlainString", "()Ljava/lang/String;", bd_plain_body);
    registry.register(bd, "intValue", "()I", native_bd_int_value);
    registry.register(bd, "longValue", "()J", native_bd_long_value);
    // Round 13 wave 12 (lane bigdec3): correctly rounded in binary, no decimal
    // rendering. `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE=0` registers the old body.
    let (bd_double_value_body, _) = bd_double_float_value_bodies();
    registry.register(bd, "doubleValue", "()D", bd_double_value_body);
    // Round 13 wave 13 (lane bigdec4; proposal BD3-1): `floatValue()` with the
    // binary body too. The JDK's `floatValue` (`BigDecimal.java:3742`) is its
    // exact fast path plus `fullFloatValue`, correctly rounded, and
    // `native_bd_float_value_binary` rounds the exact value once to `f32`, so
    // the answer is the same bit pattern; the bytecode road built
    // `bigTenToThe(scale)` and ran `divideAndRemainder` through
    // `MutableBigInteger` on every call (`floatvalue-small` was 24x HotSpot).
    // Registered only with the binary body: `CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE=0`
    // or `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE=0` leaves the JDK bytecode.
    if let Some(body) = bd_float_value_real_jdk_body() {
        registry.register(bd, "floatValue", "()F", body);
    }
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
    // `compareTo`, the exact `divide` overloads and the `MathContext`
    // overloads of `add`/`subtract`/`multiply`/`divide`/`pow`/`negate` are
    // NOT registered by the block above (RBIGDEC.1's original scope), so in
    // real-JDK mode they fell all the way through to real JDK bytecode —
    // see `docs/internal/retired/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817-RETIRED-20260917.md`,
    // "MathContext overloads have zero native registrations". The

    // similarly-named `native_bd_compare_to` / `native_bd_divide` /
    // `native_bd_divide_scale` elsewhere in this file look like they cover
    // this, but they are registered only from `register_bigdecimal_natives`,
    // which `register_synthetic_overrides` calls — `#[cfg(feature =
    // "synthetic-jdk")]`, never compiled into the default `cratonvm-cli`
    // build. They are also `f64`-based (silently wrong past ~17 significant
    // digits), so the fix here is a fresh, exact implementation, not a
    // promotion of those.
    registry.register(
        bd,
        "compareTo",
        "(Ljava/math/BigDecimal;)I",
        native_bd_compare_to_exact,
    );
    registry.register(
        bd,
        "divide",
        "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;",
        native_bd_divide_exact,
    );
    // Round 13 wave 8 (lane bigdec): one normalised division instead of one
    // digit per pass, and the right answer when the integer quotient alone
    // exceeds the precision. `CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT=0`
    // registers the old body.
    let oneshot =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT");
    let bd_divide_mc_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if oneshot {
        native_bd_divide_mc_oneshot
    } else {
        native_bd_divide_mc
    };
    registry.register(
        bd,
        "divide",
        "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;",
        bd_divide_mc_body,
    );
    registry.register(
        bd,
        "add",
        "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;",
        native_bd_add_mc,
    );
    registry.register(
        bd,
        "subtract",
        "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;",
        native_bd_subtract_mc,
    );
    registry.register(
        bd,
        "multiply",
        "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;",
        native_bd_multiply_mc,
    );
    // Round 13 wave 8 (lane bigdec): X3.274 as the JDK runs it (O(log n)
    // rounded multiplies, negative `n` allowed). `CRATONVM_BIGDECIMAL_POW_MC_JDK=0`
    // registers the old exact-power-then-round body.
    let x3274 = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_POW_MC_JDK");
    let bd_pow_mc_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if x3274 {
        native_bd_pow_mc_jdk
    } else {
        native_bd_pow_mc
    };
    registry.register(
        bd,
        "pow",
        "(ILjava/math/MathContext;)Ljava/math/BigDecimal;",
        bd_pow_mc_body,
    );
    registry.register(
        bd,
        "negate",
        "(Ljava/math/MathContext;)Ljava/math/BigDecimal;",
        native_bd_negate_mc,
    );
    registry.set_category(__prev_cat);
}

// =============================================================================
// Real-JDK-mode `compareTo` / exact `divide` / `MathContext` overloads
//
// Added 2026-09-17. `register_bigdecimal_arithmetic_overrides` above (RBIGDEC.1,
// the family actually active in real-JDK mode) never covered `compareTo`,
// the exact `divide(BigDecimal)`/`divide(BigDecimal,int,int)` overloads, or
// any `MathContext`-taking overload — those ran as real JDK bytecode. For a
// workload built entirely out of `MathContext` calls (every op in
// `LegendreHighPrecisionRuleFactory`, the witness class of
// `bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817-RETIRED-20260917.md`, uses
// `add`/`subtract`/`multiply`/`divide`/`pow` exclusively via `MathContext`,

// plus one plain `compareTo` in its bisection convergence check) that meant
// NONE of this file's fast exact-BigInt natives were ever reached; the whole
// computation ran interpreted, allocating a `MutableBigInteger`-shaped
// int[]/BigInteger temporary per digit-rounding step.
//
// Every semantic detail below (exception messages, the `result_scale =
// max(natural_terminating_scale, preferred_scale)` rule for exact divide, the
// "round once and check for a carry that grows the digit count by exactly
// one" shape of `MathContext` rounding) was pinned down against real
// `openjdk 25.0.3` on the Azure Linux box (`/tmp/bdclasses/{ExactDivProbe,
// DivZeroProbe,MCProbe,MCProbe2,MCProbe3}.java`) rather than recalled from
// memory — a first pass here trusted a half-remembered JDK algorithm shape
// for exact divide's result scale and it was wrong (real JDK pads UP to the
// preferred scale when the natural terminating scale is smaller; it does not
// strip down to the natural minimal scale).
// =============================================================================

/// Exact base-10 significant-digit count of `|u|`, matching
/// `BigDecimal.precision()`'s convention that a zero value has precision 1.
/// Same computation `native_bd_precision` already does from a decimal string;
/// factored out so the `MathContext` rounding path and the `compareTo`
/// adjusted-exponent shortcut share it instead of re-deriving it.
/// Exact base-10 significant-digit count of `|u|`, WITHOUT ever rendering a
/// decimal string. `bigint_decimal_digit_count_upper_bound` below is exact or
/// one too high — never too low — so one comparison against `10^(bound-1)`
/// (built via `bigint_pow10`'s square-and-multiply, not a decimal string)
/// settles which.
///
/// PERF (2026-09-17): this used to be `u.to_decimal().len()`. That fixed the
/// FIRST regression (see `bigint_decimal_digit_count_upper_bound`'s doc
/// comment: skip the exact count entirely when a cheap bound already proves
/// no rounding is needed) but not a second one hiding behind it — a
/// 60-digit-precision workload where intermediate exact results genuinely
/// DO need rounding on nearly every call (multiplying two ~60-digit
/// operands gives a ~120-digit product) means the bound-says-"fine" fast
/// path rarely fires, and the exact count still ran `to_decimal` on a
/// growing ~100+ digit number every time. `perf record` on
/// `LegendreHighPrecisionTest` still showed `to_decimal` at 4.15% (down from
/// 10.3%, but still the third-hottest symbol) after the first fix, entirely
/// from this path. `bigint_pow10` + one magnitude `cmp` is `O(digits)`-ish
/// (square-and-multiply) against `to_decimal`'s `O(digits²)` digit-at-a-time
/// conversion, and touches no `String`/`format!` machinery at all.
fn bigint_decimal_digit_count(u: &crate::bigint::BigInt) -> i32 {
    if u.is_zero() {
        return 1;
    }
    // Round 13 wave 12 (lane bigdec3): a magnitude of at most two limbs (every
    // compact `BigDecimal`) is counted in registers. The general road below
    // builds `10^(est-1)` by square-and-multiply for EVERY call, which made
    // `compareTo` of two small values with different scales (`1.5` vs `2`)
    // allocate two powers of ten. Exact, so no switch: `ilog10` is the digit
    // count minus one by definition.
    if u.mag_le().len() <= 2 {
        return bd_mag_low_u64(u).ilog10() as i32 + 1;
    }
    // Same wave: `log10|u|` lies in `[(bits - 1) * log10(2), bits * log10(2))`,
    // an interval 0.301 wide. When no integer falls inside it (widened by a
    // margin far above the float error, < 1e-7 for any `bits` below 2^32),
    // `floor(log10|u|)` is known and so is the count; about 70% of widths
    // decide here without building a power of ten.
    let bits = u.magnitude_bits() as f64;
    let lo = ((bits - 1.0) * std::f64::consts::LOG10_2 - 1e-6).floor() as i64;
    let hi = (bits * std::f64::consts::LOG10_2 + 1e-6).floor() as i64;
    if lo == hi {
        return lo as i32 + 1;
    }
    let est = bigint_decimal_digit_count_upper_bound(u);
    if est > 1 {
        let threshold = bigint_pow10((est - 1) as u32);
        let abs_u = if u.is_neg() { u.neg_value() } else { u.clone() };
        if abs_u.cmp(&threshold) == std::cmp::Ordering::Less {
            return est - 1;
        }
    }
    est
}

/// Cheap, CONSERVATIVE upper bound on `|u|`'s base-10 digit count, from its
/// bit length alone — no `to_decimal()`. A `k`-bit magnitude is `< 2^k <=
/// 10^(k * log10(2))`, so it has at most `floor(k * log10(2)) + 1` digits;
/// `log10(2) ≈ 0.30103` rounds up enough that this can OVER-count by one but
/// never under-count.
///
/// PERF (2026-09-17): `bd_round_to_precision` originally called
/// `bigint_decimal_digit_count` (a full `to_decimal()`, `O(digits²)`)
/// unconditionally on every `add`/`subtract`/`multiply` with `MathContext` —
/// i.e. every call `BigDecimalBench` and `LegendreHighPrecisionRuleFactory`
/// make. `to_decimal` and the `alloc::fmt` machinery behind it became the
/// single hottest symbol in a `perf record` of `BigDecimalBench` (10.3%),
/// worse than the interpreted bytecode path it replaced. The overwhelmingly
/// common case — the exact result already fits within `target_precision`,
/// so nothing needs rounding at all — never needs the exact count, only "is
/// it small enough", which this answers without ever touching a decimal
/// string.
fn bigint_decimal_digit_count_upper_bound(u: &crate::bigint::BigInt) -> i32 {
    if u.is_zero() {
        return 1;
    }
    let bits = u.magnitude_bits() as f64;
    (bits * std::f64::consts::LOG10_2).floor() as i32 + 1
}

/// Round an exact `(unscaled, scale)` pair to at most `target_precision`
/// significant digits, matching `BigDecimal`'s `MathContext` rounding
/// (`doRound`). `target_precision <= 0` means unlimited (no rounding, `mc ==
/// UNLIMITED` or a plain 0-arg ctor never reaches this — callers check
/// first). The divisor at every step is a power of ten (always positive), so
/// — unlike general `divide` — `unscaled`'s own sign IS the true quotient's
/// sign at each step; `bd_round_needs_increment` can take it directly.
///
/// Rounding a value already within `target_precision` returns it UNCHANGED,
/// scale included — MEASURED (`MCProbe.java` on real HotSpot): `10/4 @prec3`
/// is `"2.5"` (scale 1), not `"2.50"` padded out to 3 significant digits.
/// `MathContext` precision is a ceiling, never a floor.
///
/// A rounding-up increment can itself carry into one more digit than
/// requested (`9.995` rounded to 3 significant figures is `10.0`, not a
/// 4-digit `1000`) — MEASURED. That carry can only ever add exactly one
/// digit (incrementing an N-digit truncated quotient by 1 reaches at most
/// `10^N`, which is N+1 digits), so the fixup is a second, always-exact
/// (remainder-zero) division by 10, not a loop that needs a bound.
fn bd_round_to_precision(
    unscaled: &crate::bigint::BigInt,
    scale: i32,
    target_precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    if target_precision <= 0 || unscaled.is_zero() {
        return Ok((unscaled.clone(), scale));
    }
    if bigint_decimal_digit_count_upper_bound(unscaled) <= target_precision {
        // The bound is provably >= the true digit count, so this is a
        // guaranteed no-rounding-needed answer, no exact count required.
        return Ok((unscaled.clone(), scale));
    }
    let cur_precision = bigint_decimal_digit_count(unscaled);
    if cur_precision <= target_precision {
        return Ok((unscaled.clone(), scale));
    }
    let drop = cur_precision - target_precision;
    // Round 13 wave 9 (lane bigdec2): `doRound`'s
    // `scale = checkScaleNonZero((long) scale - drop)`, in the JDK's order
    // (before the division). This was `scale - drop` in `i32`: a debug panic
    // and a release wrap to a huge positive scale for a scale near
    // `Integer.MIN_VALUE`.
    let mut new_scale = bd_check_scale_non_zero(i64::from(scale) - i64::from(drop))?;
    bd_pow_ten_check(drop)?;
    let divisor = bigint_pow10(drop as u32);
    let (q, r) = unscaled.divmod(&divisor);
    let dividend_neg = unscaled.is_neg();
    let increment = bd_round_needs_increment(&r, &divisor, &q, dividend_neg, mode).map_err(|()| {
        MethodCallFailed::from(RuntimeError::ArithmeticException {
            message: "Rounding necessary".to_string(),
        })
    })?;
    let mut rounded = if increment {
        let one = BigInt::from_decimal(if dividend_neg { "-1" } else { "1" });
        q.add(&one)
    } else {
        q
    };
    // The one-digit carry fixup — see the doc comment above. Guaranteed exact
    // (remainder zero) because `rounded` can only be `10^target_precision`
    // exactly, never anything past it. `doRound`'s second pass checks the
    // scale again.
    if bigint_decimal_digit_count(&rounded) > target_precision {
        new_scale = bd_check_scale_non_zero(i64::from(new_scale) - 1)?;
        rounded = rounded.div(&BigInt::from_decimal("10"));
    }
    Ok((rounded, new_scale))
}

/// Read a `MathContext`'s `(precision, roundingMode)` pair. `MathContext` and
/// `RoundingMode` are always VM-synthetic objects (`register_phase52_math_context`,
/// `native-builtins/src/phases_early.rs`) regardless of real-JDK vs
/// synthetic-JDK mode — fixed field indices, not a real-JDK layout lookup:
/// `MathContext` field 0 is `precision:I`, field 1 is `roundingMode:Ljava/math/RoundingMode;`;
/// `RoundingMode` field 0 is its ordinal, 0-7, which the JDK deliberately
/// keeps numerically identical to the old `BigDecimal.ROUND_*` constants this
/// file's `BD_ROUND_*` already use (`RoundingMode(int oldMode)`'s declaration
/// order).
///
/// **Round 13 wave 8 (lane bigdec): the "field 0 is the ordinal" half was
/// false.** The `RoundingMode` constants are the JDK's own enum objects
/// (`canonical_enum_constant`), whose slot 0 is `Enum.name` -- a `String`
/// -- so every read fell to the `HALF_UP` default. Every `MathContext` native
/// (`add`/`subtract`/`multiply`/`divide`/`pow`/`negate`) rounded HALF_UP
/// whatever the context said: `DECIMAL32`'s HALF_EVEN ties went up
/// (`100839414/12` = `8403285`, JDK `8403284`), `UP` and `CEILING` truncated
/// or rounded to nearest. The mode is now read from `RoundingMode.oldMode`
/// (numerically the `BD_ROUND_*` constant) resolved by name, slot 0 kept as
/// the fallback for a minted stand-in (`rounding_mode_alloc` writes the
/// ordinal there); the `MathContext` slots are resolved by name too.
/// `CRATONVM_BIGDECIMAL_MC_MODE_BY_NAME=0` restores the fixed slots.
fn bd_mc_precision_and_mode(ctx: &dyn NativeContext, mc: ObjectRef) -> (i32, i32) {
    let (prec_i, rm_i, old_mode_i) = bd_mc_layout(ctx);
    let precision = match ctx.get_field(mc, prec_i) {
        Value::Int(p) => p,
        _ => 0,
    };
    let mode = match ctx.get_field(mc, rm_i) {
        Value::Object(Some(rm)) => {
            let by_name = old_mode_i.map(|i| ctx.get_field(rm, i));
            match by_name {
                Some(Value::Int(m)) => m,
                _ => match ctx.get_field(rm, 0) {
                    Value::Int(m) => m,
                    _ => BD_ROUND_HALF_UP,
                },
            }
        }
        _ => BD_ROUND_HALF_UP,
    };
    (precision, mode)
}

/// `(MathContext.precision, MathContext.roundingMode, RoundingMode.oldMode)`
/// slots, memoised per VM once all three resolve (same scoping as
/// `bd_layout`). Before that -- or with the switch off -- the historical fixed
/// slots `(0, 1, None)`.
fn bd_mc_layout(ctx: &dyn NativeContext) -> (usize, usize, Option<usize>) {
    let vm = ctx.vm_identity();
    let cached = MC_LAYOUT_TLS.with(|c| match c.get() {
        Some((owner, layout)) if owner == vm => Some(layout),
        _ => None,
    });
    if let Some(layout) = cached {
        return layout;
    }
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_MC_MODE_BY_NAME") {
        let layout = (0, 1, None);
        MC_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
        return layout;
    }
    let prec = ctx.resolve_field_index("java/math/MathContext", "precision");
    let rm = ctx.resolve_field_index("java/math/MathContext", "roundingMode");
    let old_mode = ctx.resolve_field_index("java/math/RoundingMode", "oldMode");
    match (prec, rm, old_mode) {
        (Some(p), Some(r), Some(o)) => {
            let layout = (p, r, Some(o));
            MC_LAYOUT_TLS.with(|c| c.set(Some((vm, layout))));
            layout
        }
        _ => (prec.unwrap_or(0), rm.unwrap_or(1), old_mode),
    }
}

// Round 13 wave 9 (lane bigdec2) switches that a native body consults per
// call, read once per VM per thread (same scoping as `BD_LAYOUT_TLS`). A set
// bit is a switch that is on. The switches that choose a whole body are read
// at registration instead (`register_*_arithmetic_overrides`).
thread_local! {
    static W9_SWITCHES_TLS: std::cell::Cell<Option<(usize, u8)>> =
        const { std::cell::Cell::new(None) };
}

/// `CRATONVM_BIGDECIMAL_NULL_ARG_NPE`: a null `BigDecimal`/`MathContext`
/// argument throws the JDK's `NullPointerException` instead of answering null.
const W9_NULL_ARG_NPE: u8 = 1;
/// `CRATONVM_BIGDECIMAL_EXACT_DIVIDE_BY_FACTORS`: `bd_exact_divide_core_factored`.
const W9_EXACT_DIVIDE_BY_FACTORS: u8 = 1 << 1;
/// `CRATONVM_BIGNUM_JDK_IDENTITY`: the JDK's identities -- `setScale(oldScale)`
/// and `BigInteger.pow(1)`/`ZERO.pow(n)` answer the receiver, `pow(0)` and
/// `ONE.pow(n)` answer `ONE`, `BigInteger.valueOf(-16..16)` and
/// `BigDecimal.valueOf(0..10)` answer the JDK's cached constants.
const W9_JDK_IDENTITY: u8 = 1 << 2;
/// `CRATONVM_BIGDECIMAL_ADD_MC_JDK`: `add`/`subtract` with a `MathContext`
/// take the JDK's zero-operand and `preAlign` roads (`bd_add_mc_core`).
const W9_ADD_MC_JDK: u8 = 1 << 3;
/// `CRATONVM_BIGDECIMAL_RESULT_CONSTANTS` (round 13 wave 12, lane bigdec3):
/// `add`/`subtract`/`multiply`/`negate`/`setScale` answer the JDK's cached
/// constants where its `valueOf` roads do (`bd_result_constant`).
const W12_RESULT_CONSTANTS: u8 = 1 << 4;
/// `CRATONVM_BIGDECIMAL_COMPARE_BY_BITS` (same wave): `compareTo` of values
/// whose magnitudes cannot overlap is decided from bit lengths
/// (`bd_compare_by_bits`).
const W12_COMPARE_BY_BITS: u8 = 1 << 5;
/// `CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS` (round 13 wave 13, lane bigdec4;
/// proposal BD3-3): the `MathContext` overloads, exact `divide` and
/// `toBigInteger` answer the object JDK 25 answers where that is a cached
/// constant or an operand (`bd_mc_result_constant`, `bd_add_mc_identity`).
const W13_MC_RESULT_CONSTANTS: u8 = 1 << 6;

fn w9_switch(ctx: &dyn NativeContext, bit: u8) -> bool {
    let vm = ctx.vm_identity();
    let cached = W9_SWITCHES_TLS.with(|c| match c.get() {
        Some((owner, bits)) if owner == vm => Some(bits),
        _ => None,
    });
    let bits = match cached {
        Some(bits) => bits,
        None => {
            let mut bits = 0u8;
            if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_NULL_ARG_NPE") {
                bits |= W9_NULL_ARG_NPE;
            }
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BIGDECIMAL_EXACT_DIVIDE_BY_FACTORS",
            ) {
                bits |= W9_EXACT_DIVIDE_BY_FACTORS;
            }
            if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGNUM_JDK_IDENTITY") {
                bits |= W9_JDK_IDENTITY;
            }
            if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_ADD_MC_JDK") {
                bits |= W9_ADD_MC_JDK;
            }
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BIGDECIMAL_RESULT_CONSTANTS",
            ) {
                bits |= W12_RESULT_CONSTANTS;
            }
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BIGDECIMAL_COMPARE_BY_BITS",
            ) {
                bits |= W12_COMPARE_BY_BITS;
            }
            if cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS",
            ) {
                bits |= W13_MC_RESULT_CONSTANTS;
            }
            W9_SWITCHES_TLS.with(|c| c.set(Some((vm, bits))));
            bits
        }
    };
    bits & bit != 0
}

/// The answer to a null argument the JDK dereferences: its helpful
/// `NullPointerException`, or -- with `CRATONVM_BIGDECIMAL_NULL_ARG_NPE=0` --
/// the historical `legacy` value (null, or `0` for `compareTo`).
///
/// Every `BigDecimal` native below used to answer `null` for a null operand or
/// `MathContext` (`compareTo(null)` answered `0`), where the JDK throws at its
/// first dereference; the caller got a null result and failed later,
/// elsewhere. The messages are HotSpot's helpful-NPE text for the first
/// dereference in the JDK 25 method body, naming the parameter the way the
/// transcribed `BigInteger` rows above do (`java.base` carries its parameter
/// names): `Cannot read field "precision" because "mc" is null` for every
/// `MathContext` overload (each starts `if (mc.precision == 0)`, or reaches
/// `plus(mc)`/`add(.., mc)` whose parameter is also `mc`).
fn bd_null_arg(ctx: &dyn NativeContext, message: &str, legacy: Value) -> MethodCallResult {
    if w9_switch(ctx, W9_NULL_ARG_NPE) {
        Err(RuntimeError::NullPointerException {
            message: Some(message.to_string()),
        }
        .into())
    } else {
        Ok(Some(legacy))
    }
}

const BD_NPE_MC: &str = "Cannot read field \"precision\" because \"mc\" is null";

/// The `precision == 0` arm of a `MathContext` overload delegates to the
/// exact overload, which dereferences the operand differently; this picks the
/// message by the context's precision the way the JDK body branches.
fn bd_null_operand_mc(
    ctx: &dyn NativeContext,
    mc: Option<ObjectRef>,
    exact_msg: &str,
    rounded_msg: &str,
) -> MethodCallResult {
    match mc {
        None => bd_null_arg(ctx, BD_NPE_MC, Value::Object(None)),
        Some(mc) => {
            let (precision, _) = bd_mc_precision_and_mode(ctx, mc);
            let msg = if precision == 0 { exact_msg } else { rounded_msg };
            bd_null_arg(ctx, msg, Value::Object(None))
        }
    }
}

/// Exact `BigDecimal.compareTo(BigDecimal)` -- the SHIPPING (real-JDK-mode)
/// body. It replaced an `f64` comparison that mis-orders any pair agreeing in
/// its leading ~17 significant digits, exactly the shape a Newton-Raphson
/// bisection compares near convergence.
///
/// The synthetic-JDK twin, `native_bd_compare_to`, is NOT dead: it is the live
/// body in synthetic mode, and until 2026-09-17 it still used the `f64` parse.
/// It now parses its value string exactly (`bd_parse_decimal_str`) and compares
/// through the same [`bd_compare_unscaled`], so the two modes agree.
///
/// Mirrors real `BigDecimal.compareTo`'s own safety shortcut rather than
/// unconditionally rescaling the smaller-scale operand up to the larger
/// scale: two legitimate small-magnitude values at a huge scale GAP
/// (`"1".compareTo(new BigDecimal("1E-2000000000"))` is ordinary, legal
/// `BigDecimal` usage) would otherwise force materializing a `10^2000000000`
/// `BigInt` just to answer a sign question. Compare the "adjusted exponent"
/// (`digit_count(unscaled) - scale`, i.e. decimal order of magnitude) first;
/// it decides the answer whenever the two operands differ in magnitude, and
/// when it does not, `sa - sb == pa - pb` by construction, which bounds the
/// one exact rescale this function ever performs by the operands' own digit
/// counts rather than by the caller-chosen scale.
fn native_bd_compare_to_exact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `compareTo`'s first line is `if (scale == val.scale)`.
        _ => {
            return bd_null_arg(
                ctx,
                "Cannot read field \"scale\" because \"val\" is null",
                Value::Int(0),
            )
        }
    };
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let by_bits = if w9_switch(ctx, W12_COMPARE_BY_BITS) {
        bd_compare_by_bits(&ua, sa, &ub, sb)
    } else {
        None
    };
    let cmp = by_bits.unwrap_or_else(|| bd_compare_unscaled(&ua, sa, &ub, sb));
    Ok(Some(Value::Int(match cmp {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })))
}

/// `compareTo` decided from the operands' bit lengths alone, or `None` when
/// that cannot decide it (round 13 wave 12, lane bigdec3,
/// `CRATONVM_BIGDECIMAL_COMPARE_BY_BITS`).
///
/// `log10|u * 10^-s|` lies in `[(bits - 1) * log10(2) - s, bits * log10(2) - s)`.
/// When the two values' intervals are disjoint (by a margin far above the
/// float error, < 1e-5 for any `bits` below 2^32 and any `i32` scale) their
/// magnitudes are ordered without the exact road's two digit counts, each of
/// which builds a power of ten for a wide operand. Operands of different
/// signs, a zero or equal scales are left to [`bd_compare_unscaled`], which
/// answers those without arithmetic.
fn bd_compare_by_bits(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let sign = ua.signum();
    if sign == 0 || sign != ub.signum() || sa == sb {
        return None;
    }
    const MARGIN: f64 = 1e-3;
    let c = std::f64::consts::LOG10_2;
    let (la, lb) = (ua.magnitude_bits() as f64, ub.magnitude_bits() as f64);
    let (a_lo, a_hi) = ((la - 1.0) * c - f64::from(sa), la * c - f64::from(sa));
    let (b_lo, b_hi) = ((lb - 1.0) * c - f64::from(sb), lb * c - f64::from(sb));
    let magnitude = if a_hi + MARGIN < b_lo {
        Ordering::Less
    } else if b_hi + MARGIN < a_lo {
        Ordering::Greater
    } else {
        return None;
    };
    Some(if sign > 0 {
        magnitude
    } else {
        magnitude.reverse()
    })
}

fn bd_compare_unscaled(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let xsign = ua.signum();
    let ysign = ub.signum();
    if xsign != ysign {
        return xsign.cmp(&ysign);
    }
    if xsign == 0 {
        return Ordering::Equal;
    }
    if sa == sb {
        return ua.cmp(ub);
    }
    let pa = i64::from(bigint_decimal_digit_count(ua));
    let pb = i64::from(bigint_decimal_digit_count(ub));
    let adj_a = pa - i64::from(sa);
    let adj_b = pb - i64::from(sb);
    if adj_a != adj_b {
        let cmp = adj_a.cmp(&adj_b);
        return if xsign > 0 { cmp } else { cmp.reverse() };
    }
    // Same order of magnitude: `sa - sb == pa - pb` here, so the rescale
    // below is bounded by the operands' own digit-count difference, not by
    // an arbitrary caller-chosen scale gap.
    if sa > sb {
        let raised = bigint_mul_pow10(ub, sa - sb);
        ua.cmp(&raised)
    } else {
        let raised = bigint_mul_pow10(ua, sb - sa);
        raised.cmp(ub)
    }
}

/// `Division undefined` (0/0) vs `Division by zero` (nonzero/0) — the exact
/// `divide(BigDecimal)` and `divide(BigDecimal,MathContext)` overloads' own
/// zero check, MEASURED distinct from `divide(BigDecimal,int,int)`'s plain
/// `"/ by zero"` (`DivZeroProbe.java`: that deprecated overload has no
/// special-case zero check of its own and just lets the underlying integer
/// division throw).
fn bd_divide_zero_check(
    ua: &crate::bigint::BigInt,
    ub: &crate::bigint::BigInt,
) -> Result<(), MethodCallFailed> {
    if !ub.is_zero() {
        return Ok(());
    }
    if ua.is_zero() {
        Err(RuntimeError::ArithmeticException {
            message: "Division undefined".to_string(),
        }
        .into())
    } else {
        Err(RuntimeError::ArithmeticException {
            message: "Division by zero".to_string(),
        }
        .into())
    }
}

/// Exact division core shared by `divide(BigDecimal)` and
/// `divide(BigDecimal,MathContext)` when `mc.precision() <= 0` (UNLIMITED).
/// Searches increasing scale, starting at the preferred scale
/// (`this.scale() - divisor.scale()`, saturated — never throws by itself,
/// MEASURED `saturateLong` is not `checkScale`), for the first scale at which
/// division is exact. Terminates and returns AS SOON AS the remainder is
/// zero, which is by construction the natural minimal terminating scale, or
/// the preferred scale if that is already large enough — MEASURED
/// (`ExactDivProbe.java`) this reproduces `result_scale =
/// max(natural_terminating_scale, preferred_scale)` exactly, including the
/// pad-up case (`"1.00000" / "4" = "0.25000"`, scale 5) without a separate
/// pad step.
///
/// Bounded by the divisor's own bit length plus one: past that many extra
/// decimal digits of scale, a division that has not terminated yet never
/// will (the needed extra digits equal the divisor's post-reduction count of
/// factor-2/factor-5, each individually bounded by its bit length).
fn bd_exact_divide_core(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    bd_divide_zero_check(ua, ub)?;
    let preferred_scale = saturate_i64_to_i32(i64::from(sa) - i64::from(sb));
    if ua.is_zero() {
        return Ok((BigInt::zero(), preferred_scale));
    }
    let ten = BigInt::from_decimal("10");
    let mut scale = preferred_scale;
    let (mut q, mut r) = ua.divmod(ub);
    if r.is_zero() {
        return Ok((q, scale));
    }
    // Standard long-division digit extraction: bring down one more digit by
    // multiplying the REMAINDER (not the whole numerator) by 10 and dividing
    // again. `|r| < |ub|` is the divmod invariant, so this step's cost is
    // bounded by `ub`'s own size at every iteration — not by the growing
    // scaled numerator a naive "recompute `ua * 10^step` from scratch each
    // time" loop would multiply and divide on. MEASURED: that naive version
    // (the first one written here) cost `BigDecimalBench`'s 60-digit-target
    // divide ~60 increasingly expensive passes and made the benchmark
    // SLOWER than the interpreted bytecode it replaced (1113ms -> 1744ms,
    // 50k iterations); this version fixed it (see the call site's own
    // measurement in the doc this function backs).
    let max_extra = ub.magnitude_bits() as i64 + 1;
    for _ in 0..max_extra {
        if scale == i32::MAX {
            return Err(bd_underflow());
        }
        scale += 1;
        r = r.mul(&ten);
        let (dq, nr) = r.divmod(ub);
        q = q.mul(&ten).add(&dq);
        r = nr;
        if r.is_zero() {
            return Ok((q, scale));
        }
    }
    Err(RuntimeError::ArithmeticException {
        message: "Non-terminating decimal expansion; no exact representable decimal result."
            .to_string(),
    }
    .into())
}

fn saturate_i64_to_i32(v: i64) -> i32 {
    if v > i64::from(i32::MAX) {
        i32::MAX
    } else if v < i64::from(i32::MIN) {
        i32::MIN
    } else {
        v as i32
    }
}

fn native_bd_divide_exact(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `divide(BigDecimal)`'s first line is `if (divisor.signum() == 0)`.
        _ => return bd_null_arg(ctx, BD_NPE_DIVIDE_EXACT, Value::Object(None)),
    };
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let (q, scale) = bd_exact_divide(ctx, &ua, sa, &ub, sb)?;
    // Round 13 wave 13 (BD3-3): JDK 25 `divide(BigDecimal)` answers
    // `zeroValueOf(preferredScale)` for a zero dividend, else
    // `divide(divisor, mc)` under `UNNECESSARY` -- which cannot round, so its
    // closing `doRound` never drops a digit and the quotient is a `valueOf` --
    // then that quotient or its `setScale(preferredScale)` (a `valueOf`).
    if let Some(c) = bd_mc_result_constant(ctx, &q, scale) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &q, scale);
    Ok(Some(Value::Object(Some(result?))))
}

const BD_NPE_DIVIDE_EXACT: &str =
    "Cannot invoke \"java.math.BigDecimal.signum()\" because \"divisor\" is null";

/// The exact quotient through the core `CRATONVM_BIGDECIMAL_EXACT_DIVIDE_BY_FACTORS`
/// selects (default: [`bd_exact_divide_core_factored`]).
fn bd_exact_divide(
    ctx: &dyn NativeContext,
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    if w9_switch(ctx, W9_EXACT_DIVIDE_BY_FACTORS) {
        bd_exact_divide_core_factored(ua, sa, ub, sb)
    } else {
        bd_exact_divide_core(ua, sa, ub, sb)
    }
}

/// [`bd_exact_divide_core`]'s answer -- the same quotient, scale and
/// refusals, including which of `Underflow` and `Non-terminating` wins --
/// without its digit loop (round 13 wave 9, lane bigdec2).
///
/// The loop brought down one decimal digit per pass for up to
/// `bits(divisor) + 1` passes before it gave up, each pass as wide as the
/// divisor plus the growing quotient: a 10 000-digit non-terminating divisor
/// was ~33 000 passes before `Non-terminating decimal expansion`. The answer
/// is decidable up front. Write `|ub| = 2^a * 5^b * m` with `gcd(m, 10) = 1`.
/// `ua * 10^k / ub` is an integer for some `k >= 0` iff `m | ua`, and then for
/// every `k >= K = max(a, b)`; the loop's first hit is the smallest such `k`,
/// which is `K` minus the trailing decimal zeros of the quotient at `K`
/// (every stripped zero is a smaller `k` that also divides, and a quotient at
/// the smallest `k` cannot end in 0). So: strip 2s and 5s, one divisibility
/// test, one division at scale `preferred + K`, strip zeros back towards
/// `preferred`.
///
/// The loop refused with `Underflow` when its scale reached `Integer.MAX_VALUE`
/// before it stopped; that is `preferred + k > MAX` when it terminates at `k`,
/// and `preferred + bits(ub) >= MAX` when it would have run all its passes.
fn bd_exact_divide_core_factored(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    bd_divide_zero_check(ua, ub)?;
    let preferred_scale = saturate_i64_to_i32(i64::from(sa) - i64::from(sb));
    if ua.is_zero() {
        return Ok((BigInt::zero(), preferred_scale));
    }
    let (q, r) = ua.divmod(ub);
    if r.is_zero() {
        return Ok((q, preferred_scale));
    }
    let ub_abs = ub.abs_value();
    // `ub` is non-zero here, so its lowest set bit is >= 0.
    let twos = ub_abs.lowest_set_bit().max(0) as u32;
    let mut m = ub_abs.shr(twos);
    let mut fives: u64 = 0;
    // 5^13 is the largest power of five in one limb.
    let five_13 = BigInt::from_le_words(false, vec![1_220_703_125]);
    let five = BigInt::from_le_words(false, vec![5]);
    for (step, width) in [(&five_13, 13u64), (&five, 1u64)] {
        loop {
            let (d, rem) = m.divmod(step);
            if !rem.is_zero() {
                break;
            }
            m = d;
            fives += width;
        }
    }
    let terminates = m.mag_le() == &[1u32][..] || ua.abs_value().divmod(&m).1.is_zero();
    let max_extra = ub.magnitude_bits() as i64 + 1;
    if !terminates {
        if i64::from(preferred_scale) + max_extra - 1 >= i64::from(i32::MAX) {
            return Err(bd_underflow());
        }
        return Err(RuntimeError::ArithmeticException {
            message: "Non-terminating decimal expansion; no exact representable decimal result."
                .to_string(),
        }
        .into());
    }
    let big_k = u64::from(twos).max(fives);
    let k = match i32::try_from(big_k) {
        Ok(k) if bd_pow_ten_check(k).is_ok() => k,
        // A 10^k past `BigInteger`'s range: only a divisor of hundreds of
        // megabytes gets here. Keep the loop's answer rather than a new one.
        _ => return bd_exact_divide_core(ua, sa, ub, sb),
    };
    let q = bigint_mul_pow10(ua, k).div(ub);
    // Strip back to the smallest terminating `k` (relative scale, so the
    // absolute one is never formed in `i32` before it is checked).
    let (q, k_min) = bd_strip_zeros_to_scale(q, k, 0);
    let scale = i64::from(preferred_scale) + i64::from(k_min);
    if scale > i64::from(i32::MAX) {
        return Err(bd_underflow());
    }
    Ok((q, scale as i32))
}

/// `divide(BigDecimal, MathContext)` core. Unlike the exact overload, a
/// bounded search for NATURAL termination is only the fast path — most
/// non-terminating divisions (anything not built from factors of 2 and 5)
/// must fall back to rounding at `target_precision` significant digits
/// instead of throwing.
///
/// Searches scale upward one decimal digit at a time from the preferred
/// scale, stopping at the first of: remainder zero (natural exact
/// termination — MEASURED `1/4 @prec5` keeps its natural scale rather than
/// padding to 5 digits, same as the unlimited-precision case) or the
/// quotient reaching `target_precision` significant digits (MEASURED
/// `1/3 @prec10` stops with exactly 10 digits of `3`s). A single quotient
/// can only gain one digit per scale step, so the loop can never overshoot
/// past `target_precision` digits before checking. Either way, the final
/// `(quotient, remainder, divisor=ub)` triple is rounded through the same
/// `bd_round_needs_increment` HALF_UP/HALF_EVEN/etc. logic `setScale` uses —
/// `remainder/ub` is the exact fractional tail whether or not the division
/// happens to terminate, so no extra guard digit is needed for a correct
/// rounding decision.
fn bd_divide_round_core(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
    target_precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    bd_divide_zero_check(ua, ub)?;
    let preferred_scale = saturate_i64_to_i32(i64::from(sa) - i64::from(sb));
    if ua.is_zero() {
        return Ok((BigInt::zero(), preferred_scale));
    }
    let ub_abs = if ub.is_neg() { ub.neg_value() } else { ub.clone() };
    // Sign of the TRUE (unrounded) quotient — needed for CEILING/FLOOR and
    // for the increment's direction. `ua`/`ub`'s own signs, not the
    // truncated quotient's (which can be exactly zero while the true
    // quotient is a nonzero fraction just past zero, e.g. `(-1)/8` at scale
    // 0 truncates to 0 but must still round toward -1 under FLOOR).
    let true_quotient_neg = ua.is_neg() != ub.is_neg();
    let ten = BigInt::from_decimal("10");
    let mut scale = preferred_scale;
    let (mut q, mut r) = ua.divmod(ub);
    // Same incremental long division as `bd_exact_divide_core` — see its
    // doc comment for why "recompute the whole scaled numerator from `ua`
    // every step" (the first version of this function) was a real
    // regression, not just an inefficiency: it made a 60-digit-precision
    // divide loop (this witness class's exact shape) cost 60 increasingly
    // expensive multiply+divmod passes instead of 60 bounded-size ones.
    //
    // PERF (2026-09-17): track `q`'s digit count incrementally instead of
    // calling `bigint_decimal_digit_count` (a `to_decimal()`) on every one
    // of up to `target_precision` iterations — see
    // `bigint_decimal_digit_count_upper_bound`'s doc comment; this loop is
    // the same cost shape, called once per digit PRODUCED rather than once
    // per operation. One digit is brought down per iteration by
    // construction (`q = q*10 + dq`), so the count only ever needs a single
    // exact measurement, taken once here on `q`'s small initial value.
    let mut q_digits: i32 = if q.is_zero() {
        0
    } else {
        bigint_decimal_digit_count(&q)
    };
    loop {
        if r.is_zero() {
            // Natural exact termination, found at `scale`. `q` is the EXACT
            // value here — possibly with more digits than `target_precision`
            // if termination happened to land late (`100/4` at precision 1:
            // exact at scale 0 is `25`, two digits). That is a genuine
            // digit-dropping rounding decision on `q`'s own trailing digits,
            // identical in shape to `add`/`subtract`/`multiply`'s
            // post-rounding step, so hand off to the same helper rather than
            // reimplementing it here.
            return bd_round_to_precision(&q, scale, target_precision, mode);
        }
        if q_digits >= target_precision {
            break;
        }
        if scale == i32::MAX {
            return Err(bd_underflow());
        }
        scale += 1;
        r = r.mul(&ten);
        let (dq, nr) = r.divmod(ub);
        q = q.mul(&ten).add(&dq);
        r = nr;
        q_digits = if q_digits == 0 {
            if q.is_zero() {
                0
            } else {
                1
            }
        } else {
            q_digits + 1
        };
    }
    // Reached here only with `r` nonzero and `digit_count(q) ==
    // target_precision` exactly (the loop stops the first time the digit
    // count reaches the target, and a single scale step can only ever add
    // one digit). `remainder/ub` is the exact fractional tail past `q`, so
    // one `bd_round_needs_increment` call is the complete, correct rounding
    // decision — no extra guard digit needed.
    let increment = bd_round_needs_increment(&r, &ub_abs, &q, true_quotient_neg, mode).map_err(
        |()| {
            MethodCallFailed::from(RuntimeError::ArithmeticException {
                message: "Rounding necessary".to_string(),
            })
        },
    )?;
    if !increment {
        return Ok((q, scale));
    }
    let one = BigInt::from_decimal(if true_quotient_neg { "-1" } else { "1" });
    let mut rounded = q.add(&one);
    // The one-digit carry fixup — see `bd_round_to_precision`'s doc comment.
    // `q` had exactly `target_precision` digits, so `rounded` is at most
    // `10^target_precision`, i.e. at most one digit past it, and dividing
    // that by 10 is exact.
    if bigint_decimal_digit_count(&rounded) > target_precision {
        rounded = rounded.div(&BigInt::from_decimal("10"));
        scale -= 1;
    }
    Ok((rounded, scale))
}

fn native_bd_divide_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let (q, scale) = if precision <= 0 {
        bd_exact_divide_core(&ua, sa, &ub, sb)?
    } else {
        bd_divide_round_core(&ua, sa, &ub, sb, precision, mode)?
    };
    let result = bd_alloc_bigint(ctx, &q, scale);
    Ok(Some(Value::Object(Some(result?))))
}

/// The `Some(obj)` of an object argument, `None` for a null or missing one.
fn bd_obj_arg(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn native_bd_add_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc_arg = bd_obj_arg(args, 2);
    let other = match bd_obj_arg(args, 1) {
        Some(o) => o,
        // `add(augend, mc)`: `if (mc.precision == 0) return add(augend);`,
        // else `augend.signum()`.
        None => {
            return bd_null_operand_mc(
                ctx,
                mc_arg,
                "Cannot read field \"intCompact\" because \"augend\" is null",
                "Cannot invoke \"java.math.BigDecimal.signum()\" because \"augend\" is null",
            )
        }
    };
    let Some(mc) = mc_arg else {
        return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None));
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    if w9_switch(ctx, W9_ADD_MC_JDK) {
        let (q, s) = bd_add_mc_core(&ua, sa, &ub, sb, precision, mode)?;
        let id = bd_add_mc_identity(ctx, this, other, (&ua, sa), (&ub, sb), precision, (&q, s), false);
        if let Some(r) = id {
            return Ok(Some(Value::Object(Some(r))));
        }
        let result = bd_alloc_bigint(ctx, &q, s);
        return Ok(Some(Value::Object(Some(result?))));
    }
    let s = sa.max(sb);
    let sum = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa))?
        .add(&bd_rescale_operand(&ub, i64::from(s) - i64::from(sb))?);
    let (rounded, rscale) = bd_round_to_precision(&sum, s, precision, mode)?;
    let result = bd_alloc_bigint(ctx, &rounded, rscale);
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_subtract_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc_arg = bd_obj_arg(args, 2);
    let other = match bd_obj_arg(args, 1) {
        Some(o) => o,
        // `if (mc.precision == 0) return subtract(subtrahend);`, else
        // `add(subtrahend.negate(), mc)`.
        None => {
            return bd_null_operand_mc(
                ctx,
                mc_arg,
                "Cannot read field \"intCompact\" because \"subtrahend\" is null",
                "Cannot invoke \"java.math.BigDecimal.negate()\" because \"subtrahend\" is null",
            )
        }
    };
    let Some(mc) = mc_arg else {
        return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None));
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    if w9_switch(ctx, W9_ADD_MC_JDK) {
        let nb = ub.neg_value();
        let (q, s) = bd_add_mc_core(&ua, sa, &nb, sb, precision, mode)?;
        let id = bd_add_mc_identity(ctx, this, other, (&ua, sa), (&nb, sb), precision, (&q, s), true);
        if let Some(r) = id {
            return Ok(Some(Value::Object(Some(r))));
        }
        let result = bd_alloc_bigint(ctx, &q, s);
        return Ok(Some(Value::Object(Some(result?))));
    }
    let s = sa.max(sb);
    let diff = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa))?
        .sub(&bd_rescale_operand(&ub, i64::from(s) - i64::from(sb))?);
    let (rounded, rscale) = bd_round_to_precision(&diff, s, precision, mode)?;
    let result = bd_alloc_bigint(ctx, &rounded, rscale);
    Ok(Some(Value::Object(Some(result?))))
}

/// The receiver's `checkScale(long)` (`BigDecimal.java:4568`) for a NON-ZERO
/// receiver: an out-of-`int` value clamps to `Integer.MAX_VALUE`/`MIN_VALUE`
/// and throws by the clamped sign.
fn bd_check_scale_nonzero_receiver(v: i64) -> Result<i32, MethodCallFailed> {
    match i32::try_from(v) {
        Ok(s) => Ok(s),
        Err(_) if v > 0 => Err(bd_underflow()),
        Err(_) => Err(RuntimeError::ArithmeticException {
            message: "Overflow".to_string(),
        }
        .into()),
    }
}

/// `BigDecimal.add(BigDecimal augend, MathContext mc)` (`BigDecimal.java`,
/// JDK 25) as the JDK runs it (round 13 wave 9, lane bigdec2,
/// `CRATONVM_BIGDECIMAL_ADD_MC_JDK`); `subtract(x, mc)` is
/// `add(x.negate(), mc)`.
///
/// The body it replaces rescaled both operands to the larger scale and
/// rounded the exact sum. That is the same VALUE on ordinary operands, but it
/// builds `10^gap` for a scale gap the JDK never materialises:
/// `ONE.add(new BigDecimal("1E-100000000"), DECIMAL64)` multiplied out a
/// 100-million-digit power of ten where HotSpot answers
/// `1.000000000000000` at once, and a gap of `715827883` or more THREW
/// `BigInteger would overflow supported range` where HotSpot answers. The JDK
/// has two roads that avoid it, transcribed here:
///
/// * a zero operand: `doRound` the other one, then move it towards the
///   preferred scale `max(scales)` only as far as the precision allows;
/// * `preAlign`: when the smaller operand's digits all lie more than two
///   places below both the larger operand's last digit and the result's
///   estimated ulp, it is replaced by `±1` three places below the larger of
///   the two -- the same rounding decision, at a bounded width.
///
/// Then `matchScale` (the operand with the smaller scale is raised, with
/// `setScale`'s refusals, i.e. [`bd_rescale_operand`]) and `doRound`.
fn bd_add_mc_core(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
    precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    if precision <= 0 {
        // `mc.precision == 0`: the exact `add(augend)`.
        let s = sa.max(sb);
        let sum = bd_rescale_operand(ua, i64::from(s) - i64::from(sa))?
            .add(&bd_rescale_operand(ub, i64::from(s) - i64::from(sb))?);
        return Ok((sum, s));
    }
    if ua.is_zero() || ub.is_zero() {
        let preferred = sa.max(sb);
        if ua.is_zero() && ub.is_zero() {
            return Ok((BigInt::zero(), preferred));
        }
        let (x, xs) = if ua.is_zero() { (ub, sb) } else { (ua, sa) };
        let (r, rs) = bd_round_to_precision(x, xs, precision, mode)?;
        if rs == preferred {
            return Ok((r, rs));
        }
        if rs > preferred {
            return Ok(bd_strip_zeros_to_scale(r, rs, preferred));
        }
        // `result.setScale(preferred)` when the precision has room for all of
        // it, else `result.setScale(result.scale() + precisionDiff)`. The
        // raise is at most `precision` places and the target scale at most
        // `preferred`, so neither leaves `int`.
        let precision_diff = i64::from(precision) - i64::from(bigint_decimal_digit_count(&r));
        let scale_diff = i64::from(preferred) - i64::from(rs);
        let raise = precision_diff.min(scale_diff).max(0) as i32;
        bd_pow_ten_check(raise)?;
        return Ok((bigint_mul_pow10(&r, raise), rs + raise));
    }
    let padding = i64::from(sa) - i64::from(sb);
    let (mut xa, mut xsa) = (ua.clone(), sa);
    let (mut xb, mut xsb) = (ub.clone(), sb);
    if padding != 0 {
        // `preAlign`: `big` is the operand with the SMALLER scale.
        let lhs_is_big = padding < 0;
        let (big, bigs, small, smalls) = if lhs_is_big {
            (ua, sa, ub, sb)
        } else {
            (ub, sb, ua, sa)
        };
        let est_ulp_scale =
            i64::from(bigs) - i64::from(bigint_decimal_digit_count(big)) + i64::from(precision);
        let small_high = i64::from(smalls) - i64::from(bigint_decimal_digit_count(small)) + 1;
        if small_high > i64::from(bigs) + 2 && small_high > est_ulp_scale + 2 {
            // `BigDecimal.valueOf(small.signum(), this.checkScale(max(..) + 3))`;
            // `this` is the (non-zero) receiver.
            let tiny_scale =
                bd_check_scale_nonzero_receiver(i64::from(bigs).max(est_ulp_scale) + 3)?;
            let tiny = BigInt::from_decimal(if small.is_neg() { "-1" } else { "1" });
            if lhs_is_big {
                xb = tiny;
                xsb = tiny_scale;
            } else {
                xa = tiny;
                xsa = tiny_scale;
            }
        }
    }
    let s = xsa.max(xsb);
    let sum = bd_rescale_operand(&xa, i64::from(s) - i64::from(xsa))?
        .add(&bd_rescale_operand(&xb, i64::from(s) - i64::from(xsb))?);
    bd_round_to_precision(&sum, s, precision, mode)
}

fn native_bd_multiply_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc_arg = bd_obj_arg(args, 2);
    let other = match bd_obj_arg(args, 1) {
        Some(o) => o,
        // Both roads read `multiplicand.scale` first.
        None => {
            return bd_null_operand_mc(ctx, mc_arg, BD_NPE_MULTIPLY, BD_NPE_MULTIPLY);
        }
    };
    let Some(mc) = mc_arg else {
        return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None));
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = bd_product_scale(&ua, sa, sb)?;
    let prod = ua.mul(&ub);
    let (rounded, rscale) = bd_round_to_precision(&prod, s, precision, mode)?;
    if let Some(c) = bd_multiply_mc_constant(ctx, (&ua, &ub, &prod), precision, (&rounded, rscale)) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &rounded, rscale);
    Ok(Some(Value::Object(Some(result?))))
}

/// `pow(int, MathContext)`. This witness class only ever calls it with a
/// small fixed exponent (`c.pow(2, mContext)`), so the exact-then-round-once
/// approach below is unconditionally correct — it is mathematically the same
/// operation real JDK's own incremental-squaring-with-intermediate-rounding
/// implementation is built to reproduce exactly (that structure exists there
/// purely so a huge exponent does not build an exponentially large
/// intermediate; rounding only at the end never disagrees for a small,
/// fixed-size exponent). No native for the exponent-less `pow(int)` overload
/// exists here since nothing in this file's scope calls it — `pow(I)` on
/// `BigInteger` is the one that already has a native (`native_bi_pow`).
fn native_bd_pow_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::bigint::BigInt;
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let mc = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    if n < 0 {
        return Err(RuntimeError::ArithmeticException {
            message: "Invalid operation".to_string(),
        }
        .into());
    }
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    if n == 0 {
        let result = bd_alloc_bigint(ctx, &BigInt::from_decimal("1"), 0);
        return Ok(Some(Value::Object(Some(result?))));
    }
    let new_scale_i64 = i64::from(sa) * i64::from(n);
    if new_scale_i64.abs() > i64::from(i32::MAX) {
        return Err(bd_underflow());
    }
    // Argument-driven allocation guard (same shape as `BigInteger.pow`'s —
    // reused directly, since `ua^n` here is exactly that computation): `n`
    // is caller-chosen and unbounded, and a naive loop would build an
    // arbitrarily large intermediate before rounding ever gets a chance to
    // shrink it back down.
    bi_pow_check_range(&ua, n)?;
    let mut acc = BigInt::from_decimal("1");
    for _ in 0..n {
        acc = acc.mul(&ua);
    }
    let (rounded, rscale) = bd_round_to_precision(&acc, new_scale_i64 as i32, precision, mode)?;
    let result = bd_alloc_bigint(ctx, &rounded, rscale);
    Ok(Some(Value::Object(Some(result?))))
}

const BD_NPE_MULTIPLY: &str = "Cannot read field \"scale\" because \"multiplicand\" is null";

fn native_bd_negate_mc(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `negate().plus(mc)`: `plus` reads `mc.precision`.
        _ => return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None)),
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let negated = ua.neg_value();
    let (rounded, rscale) = bd_round_to_precision(&negated, sa, precision, mode)?;
    if let Some(c) = bd_negate_mc_constant(ctx, &ua, precision, (&rounded, rscale)) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &rounded, rscale);
    Ok(Some(Value::Object(Some(result?))))
}

// =============================================================================
// Round 13 wave 8 (lane bigdec): `pow(int, MathContext)` computed the way the
// JDK computes it, `divide(BigDecimal, MathContext)` in one division, and a
// `toString()` that keeps the JDK's `stringCache`. Each is chosen at
// registration by its own switch; `0` registers the body it replaced.
// `r13w8-bigdec-bignum-natives-quadratic-paths-and-wrong-mathcontext-divide-FIXED-20260928.md`
// (docs/known-issues/jit) has the reasoning and the reproducers.
// =============================================================================

/// `ArithmeticException("Invalid operation")` -- `BigDecimal.pow`'s refusal.
fn bd_invalid_operation() -> MethodCallFailed {
    RuntimeError::ArithmeticException {
        message: "Invalid operation".to_string(),
    }
    .into()
}

/// `BigDecimal.checkScaleNonZero(long)` (`BigDecimal.java:4674`). The message
/// is chosen by the sign of the TRUNCATED `int`, not of the `long`:
/// `asInt > 0 ? "Underflow" : "Overflow"`.
fn bd_check_scale_non_zero(v: i64) -> Result<i32, MethodCallFailed> {
    let as_int = v as i32;
    if i64::from(as_int) == v {
        return Ok(as_int);
    }
    Err(RuntimeError::ArithmeticException {
        message: if as_int > 0 { "Underflow" } else { "Overflow" }.to_string(),
    }
    .into())
}

/// `BigDecimal.longDigitLength` of a non-negative exponent magnitude.
fn bd_u32_digit_length(v: u32) -> i32 {
    let mut digits = 1;
    let mut x = v;
    while x >= 10 {
        x /= 10;
        digits += 1;
    }
    digits
}

/// `BigDecimal.pow(int)` (`BigDecimal.java:2440`) on an exact
/// `(unscaled, scale)`: the range refusal, the receiver's `checkScale` of
/// `scale * n` (a zero receiver clamps instead of throwing), then
/// `inflated().pow(n)`.
fn bd_pow_exact_core(
    ua: &crate::bigint::BigInt,
    sa: i32,
    n: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    if !(0..=999_999_999).contains(&n) {
        return Err(bd_invalid_operation());
    }
    let wide = i64::from(sa) * i64::from(n);
    let new_scale = match i32::try_from(wide) {
        Ok(s) => s,
        Err(_) if ua.is_zero() => {
            if wide > 0 {
                i32::MAX
            } else {
                i32::MIN
            }
        }
        Err(_) => {
            return Err(RuntimeError::ArithmeticException {
                message: if wide > 0 { "Underflow" } else { "Overflow" }.to_string(),
            }
            .into())
        }
    };
    Ok((bigint_pow_checked(ua, n)?, new_scale))
}

/// One `acc.multiply(x, workmc)` of `pow(int, MathContext)`: the receiver's
/// `checkScale` of the scale sum, the exact product, then `doRound` -- the
/// same three steps `native_bd_multiply_mc` takes, which is what that call
/// dispatches to in this VM.
fn bd_multiply_round(
    a: &(crate::bigint::BigInt, i32),
    b: &(crate::bigint::BigInt, i32),
    precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    let s = bd_product_scale(&a.0, a.1, b.1)?;
    let p = a.0.mul(&b.0);
    bd_round_to_precision(&p, s, precision, mode)
}

/// `BigDecimal.pow(int, MathContext)` (`BigDecimal.java:2500`), the ANSI
/// X3.274 algorithm transcribed: squarings and multiplies into one
/// accumulator, each rounded to `mc.precision + elength + 1` digits, the
/// reciprocal at that precision for a negative `n`, then one rounding to
/// `mc.precision`.
///
/// The body this replaces (`native_bd_pow_mc`, kept for
/// `CRATONVM_BIGDECIMAL_POW_MC_JDK=0`) multiplied `n` times at full width and
/// rounded once. That is O(n^2 * |this|) limb work -- `1.0001.pow(100000,
/// DECIMAL64)` built a ~400 000-digit exact power where the JDK does ~34
/// multiplies of ~20-digit numbers -- and its answer is not the JDK's: the
/// intermediate roundings are part of X3.274's result ("within two ulps"),
/// so the last digit can differ, and a negative `n` threw
/// `Invalid operation` where the JDK returns the reciprocal.
fn bd_pow_mc_core(
    ua: &crate::bigint::BigInt,
    sa: i32,
    n: i32,
    precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    if precision == 0 {
        return bd_pow_exact_core(ua, sa, n);
    }
    if !(-999_999_999..=999_999_999).contains(&n) {
        return Err(bd_invalid_operation());
    }
    let one = BigInt::from_decimal("1");
    if n == 0 {
        return Ok((one, 0));
    }
    let mag = n.unsigned_abs();
    let work_precision = if precision > 0 {
        let elength = bd_u32_digit_length(mag);
        if elength > precision {
            return Err(bd_invalid_operation());
        }
        // `new MathContext(mc.precision + elength + 1, ..)`: an `int` sum that
        // wraps negative past Integer.MAX_VALUE, which the constructor refuses.
        match precision.checked_add(elength + 1) {
            Some(p) => p,
            None => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Digits < 0".to_string(),
                }
                .into())
            }
        }
    } else {
        precision
    };
    let lhs = (ua.clone(), sa);
    let mut acc = (one.clone(), 0i32);
    // Java's `mag += mag; if (mag < 0)` walks bits 30..0 of `|n|` (bit 31 is
    // zero for |n| <= 999_999_999); a `u32` shift reads the same bit.
    let mut bits = mag;
    let mut seenbit = false;
    for i in 1..=31 {
        bits <<= 1;
        if bits & 0x8000_0000 != 0 {
            seenbit = true;
            acc = bd_multiply_round(&acc, &lhs, work_precision, mode)?;
        }
        if i == 31 {
            break;
        }
        if seenbit {
            acc = bd_multiply_round(&acc, &acc, work_precision, mode)?;
        }
    }
    if n < 0 {
        acc = if work_precision > 0 {
            bd_divide_round_core_oneshot(&one, 0, &acc.0, acc.1, work_precision, mode)?
        } else {
            bd_exact_divide_core(&one, 0, &acc.0, acc.1)?
        };
    }
    bd_round_to_precision(&acc.0, acc.1, precision, mode)
}

fn native_bd_pow_mc_jdk(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let n = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let mc = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        // `pow(n, mc)`'s first line is `if (mc.precision == 0)`.
        _ => return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None)),
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (q, scale) = bd_pow_mc_core(&ua, sa, n, precision, mode)?;
    let result = bd_alloc_bigint(ctx, &q, scale);
    Ok(Some(Value::Object(Some(result?))))
}

/// `createAndStripZerosToMatchScale`: drop trailing decimal zeros from `q`
/// while its scale stays above `preferred` (nine at a time first, so a
/// terminating quotient padded out to thousands of digits is not stripped
/// one `divmod` per digit).
fn bd_strip_zeros_to_scale(
    q: crate::bigint::BigInt,
    scale: i32,
    preferred: i32,
) -> (crate::bigint::BigInt, i32) {
    use crate::bigint::BigInt;
    let mut q = q;
    let mut scale = scale;
    if q.is_zero() {
        return (q, scale);
    }
    let billion = BigInt::from_decimal("1000000000");
    while i64::from(scale) - i64::from(preferred) >= 9 {
        let (d, r) = q.divmod(&billion);
        if !r.is_zero() {
            break;
        }
        q = d;
        scale -= 9;
    }
    let ten = BigInt::from_decimal("10");
    while scale > preferred {
        let (d, r) = q.divmod(&ten);
        if !r.is_zero() {
            break;
        }
        q = d;
        scale -= 1;
    }
    (q, scale)
}

/// `divide(BigDecimal, MathContext)` for `mc.precision > 0`, the JDK's way
/// (`BigDecimal.java:5543`, `divide(BigInteger, int, BigInteger, int, long,
/// MathContext)`): normalise so the quotient has exactly `mcp` digits, ONE
/// division at scale `scl = preferred + yprec - xprec + mcp`, round by the
/// remainder, strip zeros towards the preferred scale when exact, `doRound`.
///
/// Replaces `bd_divide_round_core` (kept for
/// `CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT=0`), which brought down one
/// decimal digit per iteration -- `mcp` multiply/divide/allocate rounds, each
/// as wide as the divisor and the growing quotient, so a 10 000-digit
/// `MathContext` divide was 10 000 passes -- and which answered WRONG when
/// the integer quotient alone already had more than `mcp` digits: its loop
/// stopped at once and rounded the whole over-long quotient as if it had
/// `mcp` digits. `new BigDecimal(12345).divide(new BigDecimal(7), new
/// MathContext(2))` returned `1.76E+3`; the JDK says `1.8E+3`.
fn bd_divide_round_core_oneshot(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
    target_precision: i32,
    mode: i32,
) -> Result<(crate::bigint::BigInt, i32), MethodCallFailed> {
    use crate::bigint::BigInt;
    use std::cmp::Ordering;
    bd_divide_zero_check(ua, ub)?;
    let preferred = i64::from(sa) - i64::from(sb);
    if ua.is_zero() {
        // `zeroValueOf(saturateLong(preferredScale))`.
        return Ok((BigInt::zero(), saturate_i64_to_i32(preferred)));
    }
    if target_precision <= 0 {
        return bd_exact_divide_core(ua, sa, ub, sb);
    }
    let mcp = i64::from(target_precision);
    let xprec = i64::from(bigint_decimal_digit_count(ua));
    let mut yprec = i64::from(bigint_decimal_digit_count(ub));
    let ua_abs = ua.abs_value();
    let ub_abs = ub.abs_value();
    // `compareMagnitudeNormalized(x, xprec, y, yprec) > 0` -> `yscale -= 1`,
    // so that 0.1 < x'/y' <= 1 and the quotient below has exactly `mcp`
    // digits (10^mcp only when x' == y', which is exact).
    let x_above_y = if xprec >= yprec {
        ua_abs.cmp(&bigint_mul_pow10(&ub_abs, (xprec - yprec) as i32)) == Ordering::Greater
    } else {
        bigint_mul_pow10(&ua_abs, (yprec - xprec) as i32).cmp(&ub_abs) == Ordering::Greater
    };
    if x_above_y {
        yprec -= 1;
    }
    let scl = bd_check_scale_non_zero(preferred + yprec - xprec + mcp)?;
    let raise = bd_check_scale_non_zero(mcp + yprec - xprec)?;
    let (num, den) = if raise > 0 {
        bd_pow_ten_check(raise)?;
        (bigint_mul_pow10(ua, raise), ub.clone())
    } else {
        let new_scale = bd_check_scale_non_zero(xprec - mcp)?;
        let raise_y = bd_check_scale_non_zero(i64::from(new_scale) - yprec)?;
        bd_pow_ten_check(raise_y)?;
        (ua.clone(), bigint_mul_pow10(ub, raise_y))
    };
    let preferred = bd_check_scale_non_zero(preferred)?;
    let (q, r) = num.divmod(&den);
    if r.is_zero() {
        let (q, s) = bd_strip_zeros_to_scale(q, scl, preferred);
        // `doRound` only acts on the exact 10^mcp quotient (mcp + 1 digits).
        return bd_round_to_precision(&q, s, target_precision, mode);
    }
    let true_quotient_neg = ua.is_neg() != ub.is_neg();
    let den_abs = den.abs_value();
    let increment =
        bd_round_needs_increment(&r, &den_abs, &q, true_quotient_neg, mode).map_err(|()| {
            MethodCallFailed::from(RuntimeError::ArithmeticException {
                message: "Rounding necessary".to_string(),
            })
        })?;
    let q = if increment {
        q.add(&BigInt::from_decimal(if true_quotient_neg {
            "-1"
        } else {
            "1"
        }))
    } else {
        q
    };
    // A rounding carry out of `mcp` nines is 10^mcp: `doRound` drops the
    // (zero) extra digit.
    bd_round_to_precision(&q, scl, target_precision, mode)
}

fn native_bd_divide_mc_oneshot(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mc_arg = bd_obj_arg(args, 2);
    let other = match bd_obj_arg(args, 1) {
        Some(o) => o,
        // `int mcp = mc.precision; if (mcp == 0) return divide(divisor);`,
        // else `(long)dividend.scale - divisor.scale`.
        None => {
            return bd_null_operand_mc(
                ctx,
                mc_arg,
                BD_NPE_DIVIDE_EXACT,
                "Cannot read field \"scale\" because \"divisor\" is null",
            )
        }
    };
    let Some(mc) = mc_arg else {
        return bd_null_arg(ctx, BD_NPE_MC, Value::Object(None));
    };
    let (precision, mode) = bd_mc_precision_and_mode(ctx, mc);
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let (q, scale) = if precision <= 0 {
        bd_exact_divide(ctx, &ua, sa, &ub, sb)?
    } else {
        bd_divide_round_core_oneshot(&ua, sa, &ub, sb, precision, mode)?
    };
    // `precision <= 0` is exact `divide(divisor)` (see `native_bd_divide_exact`).
    let constant = if precision <= 0 {
        bd_mc_result_constant(ctx, &q, scale)
    } else {
        bd_divide_mc_constant(ctx, (&ua, sa), (&ub, sb), precision, (&q, scale))
    };
    if let Some(c) = constant {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &q, scale);
    Ok(Some(Value::Object(Some(result?))))
}

/// The slot of `BigDecimal.stringCache` in the real-JDK layout, memoised per
/// VM like `bd_layout`. `None` in the synthetic-JDK layout, which has no
/// such field.
fn bd_string_cache_index(ctx: &dyn NativeContext) -> Option<usize> {
    let vm = ctx.vm_identity();
    let cached = BD_STRING_CACHE_TLS.with(|c| match c.get() {
        Some((owner, idx)) if owner == vm => Some(idx),
        _ => None,
    });
    if cached.is_some() {
        return cached;
    }
    bd_layout(ctx)?;
    let idx = ctx.resolve_field_index("java/math/BigDecimal", "stringCache")?;
    BD_STRING_CACHE_TLS.with(|c| c.set(Some((vm, idx))));
    Some(idx)
}

/// `BigDecimal.toString()` as the JDK writes it (`BigDecimal.java:3356`):
///
/// ```java
///     String sc = stringCache;
///     if (sc == null) stringCache = sc = layoutChars(true);
///     return sc;
/// ```
///
/// `native_bd_to_string` (kept for `CRATONVM_BIGDECIMAL_TOSTRING_CACHE=0`)
/// re-rendered on every call -- an O(digits^2) limb-to-decimal conversion of
/// the unscaled value each time, where HotSpot answers a repeated
/// `toString()` of a wide value with one field load -- and it INTERNED the
/// result, so `bd.toString() == "1.5"` was `true` (HotSpot: `false`) and
/// every distinct rendering went into the intern pool. The rendering here is
/// a fresh, uninterned `String`, stored once in the receiver's own field.
fn native_bd_to_string_cached(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cache = bd_string_cache_index(ctx);
    if let Some(idx) = cache {
        if let Value::Object(Some(s)) = ctx.get_field(this, idx) {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    let s = bd_read_canonical(ctx, this);
    let Some(idx) = cache else {
        let java_str = ctx.create_string_uninterned(&s);
        return Ok(Some(Value::Object(Some(java_str))));
    };
    // GC-SAFETY: the String allocation can move `this`; pin it across.
    let h = ctx.pin_native_root(this);
    let java_str = ctx.create_string_uninterned(&s);
    let this = ctx.read_native_pin(h, this);
    ctx.set_field(this, idx, Value::Object(Some(java_str)));
    ctx.unpin_native_roots(h);
    Ok(Some(Value::Object(Some(java_str))))
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
    // `divide`/`mod`/`remainder` are NOT registered here — deleted
    // 2026-09-17, same shape as the `add`/`subtract`/`multiply` deletion
    // above: `phases_late::register_p71_biginteger_extras` registers a
    // word-based limb twin for all three, LATER than this function runs in
    // every mode that calls both (`register_builtins` = essentials, which
    // includes `register_p71_biginteger_extras`, THEN
    // `register_synthetic_overrides`, which called this function) — so
    // `register`'s last-write-wins/`slot_by_key` semantics
    // (`native-api/src/registry.rs`) meant this decimal-string trio always
    // WON the slot in synthetic-JDK mode, never the reverse. That is not
    // merely slower: this trio registered the SAME callback (`native_bi_mod`,
    // now deleted) for both `mod` and `remainder`, so
    // `BigInteger.remainder()` on mixed-sign operands answered `mod()`'s
    // always-non-negative result instead of following the dividend's sign —
    // wrong per spec, in synthetic mode only (real-JDK mode never reaches
    // this function; see `register_synthetic_overrides`'s one call site in
    // `vm_init.rs`, gated on `config.use_synthetic_jdk`).
    // `docs/internal/jdk-only/biginteger-divide-mod-remainder-shadow-confined-to-synthetic-mode-20260917-FIXED-20260917.md`.
    registry.register(bi, "negate", "()Ljava/math/BigInteger;", native_bi_negate);
    registry.register(bi, "abs", "()Ljava/math/BigInteger;", native_bi_abs);
    registry.register(
        bi,
        "compareTo",
        "(Ljava/math/BigInteger;)I",
        native_bi_compare_to,
    );
    registry.register(bi, "equals", "(Ljava/lang/Object;)Z", native_bi_equals);
    // Round 13 wave 13 (lane bigdec4): the synthetic JDK took the interning
    // body unconditionally; it now follows the shipping registrar's switch.
    let bi_to_string_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGNUM_STRINGS_UNINTERNED") {
            native_bi_to_string_uninterned
        } else {
            native_bi_to_string
        };
    registry.register(bi, "toString", "()Ljava/lang/String;", bi_to_string_body);
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
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")?))))
    });
    registry.register(bi, "ONE", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "1")?))))
    });
    registry.register(bi, "TEN", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "10")?))))
    });
    registry.register(bi, "TWO", "()Ljava/math/BigInteger;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bi_alloc(ctx, "2")?))))
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
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, &b)?))));
            }
            if b == "0" {
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, &a)?))));
            }
            while b != "0" {
                let t = b.clone();
                b = bi_mod_unsigned(&a, &t);
                a = t;
            }
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &a)?))))
        },
    );

    // `shiftLeft` / `shiftRight` are NOT registered here either, as of lane F7
    // — see the "REGISTRATION CONSOLIDATION" note below. Lane E38-1 kept them
    // only because `phases_late`'s twin had no range guard and
    // `x.shiftRight(Integer.MIN_VALUE)` there was a `vec![0u32; 67_108_864]`
    // (~256 MB) allocation. Lane F2 has since landed that guard
    // (`phases_late::p71_bi_checked_shl`), so the stated reason for keeping a
    // second copy is gone and keeping it would leave TWO live registrations for
    // one triple with registration order deciding which guard runs.

    // `and` / `or` / `xor` / `bitLength` / `bitCount` / `testBit` are NOT
    // registered here on purpose — see the
    // "REGISTRATION CONSOLIDATION" note below. The decimal
    // versions that used to live here read `bi_read(..).trim_start_matches('-')`,
    // i.e. they threw the SIGN away before computing, so `(-1) & 5` answered 1
    // instead of 5 and `(-9).bitCount()` answered 2 instead of 1; `testBit`
    // looped `0..bit`, so a negative bit address silently answered instead of
    // raising `ArithmeticException`. Every one of them had a correct
    // two's-complement limb twin already registered (earlier, and in BOTH JDK
    // modes) by `phases_late::register_p71_biginteger_extras`, which these
    // re-registrations were shadowing in synthetic-jdk mode.
    // `not` and `toByteArray` are NOT registered here either, for the same
    // reason. The decimal `not` mapped `~(-1)` to -1 instead of 0 (the
    // `neg_result == "0"` arm fired on the one input where `-(this+1)` is
    // legitimately zero), and the decimal `toByteArray` sign-extended a
    // negative through `i128::to_be_bytes` and then trimmed one byte too few,
    // so `(-1).toByteArray()` was `{0xFF, 0xFF}` where HotSpot gives `{0xFF}`.
    // `phases_late::register_p71_biginteger_extras` registers both, earlier and
    // in both modes; its `toByteArray` calls `bi_to_byte_array_str` — the
    // CORRECT converter, which lives in THIS file and which this registration
    // was shadowing (`[1 of 10 callsites]` inside one class).

    // valueOf(long) — create from long value
    registry.register(bi, "valueOf", "(J)Ljava/math/BigInteger;", |ctx, args| {
        let val = match args.first() {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        Ok(Some(Value::Object(Some(bi_alloc(ctx, &val.to_string())?))))
    });

    // isProbablePrime(certainty) — JDK 25 `BigInteger.java:1156-1166`:
    //
    //     if (certainty <= 0) return true;
    //     BigInteger w = this.abs();
    //     if (w.equals(TWO)) return true;
    //     if (!w.testBit(0) || w.equals(ONE)) return false;
    //     return w.primeToCertainty(certainty, null);
    //
    // This body used to be trial division by 5,7,11,… while `i*i <= |this|`
    // with a HARD CAP of `i <= 10000`, and it answered **1** when the cap was
    // reached. So every composite whose smallest factor exceeds 10,000 was
    // reported PRIME. MEASURED (`scratchpad/f7/Prime.java`, Microsoft OpenJDK
    // 25.0.3+9):
    //
    //     (1000003*1000033).isProbablePrime(100) = false   <- this body said true
    //     (p256*q256).isProbablePrime(100)       = false   <- this body said true
    //     4.isProbablePrime(0)  = true    4.isProbablePrime(-1) = true   <- ignored
    //     (-7).isProbablePrime(10) = true    (-4).isProbablePrime(10) = false
    //     2.isProbablePrime(10) = true       (-2).isProbablePrime(10) = true
    //     0/1/(-1).isProbablePrime(10) = false
    //
    // "Composite reported prime" is the direction that matters: it is what a
    // key-generation path trusts. `BigInt::is_probable_prime` (trial division
    // below 1000, then Miller-Rabin over 13 fixed bases) is the validated
    // primitive — `bigint::tests::is_probable_prime_matches_decimal` pins the
    // p*q semiprime case specifically — and `phases_late`'s twin already calls
    // it. It is called here on the ABSOLUTE value, which the twin does not do:
    // its `if self.neg { return false }` makes `(-7).isProbablePrime(10)`
    // false where HotSpot says true (see this lane's record, NOMINATION 3).
    registry.register(bi, "isProbablePrime", "(I)Z", |ctx, args| {
        let certainty = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if certainty <= 0 {
            return Ok(Some(Value::Int(1)));
        }
        let v = bi_read_int(ctx, obj_arg(args, 0)?);
        let w = if v.is_neg() { v.neg_value() } else { v };
        Ok(Some(Value::Int(if w.is_probable_prime() { 1 } else { 0 })))
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
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")?))));
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
            Ok(Some(Value::Object(Some(bi_alloc(ctx, &result)?))))
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
                return Ok(Some(Value::Object(Some(bi_alloc(ctx, "0")?))));
            }
            match bi_mod_inverse_str(&a_str, &m_str) {
                Some(inv) => Ok(Some(Value::Object(Some(bi_alloc(ctx, &inv)?)))),
                None => Err(RuntimeError::ArithmeticException {
                    message: "BigInteger not invertible.".to_string(),
                }
                .into()),
            }
        },
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// BigInteger shift contract
//
// REGISTRATION CONSOLIDATION (2026-08-13, lane E38). `java/math/BigInteger` is
// registered by TWO registrars and `NativeMethodRegistry::register` is
// last-registration-wins:
//
//   * `phases_late::register_p71_biginteger_extras`, reached from
//     `register_essential_natives` (native-builtins/src/lib.rs:7913) — runs in
//     EVERY jdk mode, and computes on `crate::bigint::BigInt` limbs with full
//     two's-complement semantics;
//   * `register_biginteger_natives` (this file), reached only from
//     `register_synthetic_overrides` (lib.rs:24010, `#[cfg(feature =
//     "synthetic-jdk")]`) — runs LATER, so in synthetic-jdk mode its
//     decimal-string bodies OVERWROTE the limb ones.
//
// Fourteen triples overlapped. The eight whose decimal body was demonstrably
// wrong (`and`/`or`/`xor`/`not`/`bitLength`/`bitCount`/`testBit`/
// `toByteArray` — all of them sign-stripping, see the notes at their former
// sites) are simply no longer registered here, so the limb twin wins in both
// modes and there is ONE implementation per triple.
//
// 2026-08-13, lane F7 — `shiftLeft`/`shiftRight` now go the same way, and the
// call ORDER above is worth stating exactly because two lanes have now guessed
// at it. Traced through `vm/src/vm/vm_init.rs`:
//
//   * `if config.use_synthetic_jdk` (vm_init.rs:1932) calls `register_builtins`
//     (vm_init.rs:1934), which is `register_essential_natives` THEN
//     `register_synthetic_overrides` (lib.rs:21596-21601). So in synthetic mode
//     BOTH registrars run and THIS FILE'S runs SECOND — it wins.
//   * the `else` arm (vm_init.rs:2055) and the whole
//     `#[cfg(not(feature = "synthetic-jdk"))]` build (vm_init.rs:2593) call
//     `register_essential_natives_with_shims` and NOT
//     `register_synthetic_overrides`. So in real-JDK mode this file's
//     registrar never runs at all and `phases_late`'s is the only one.
//
// E38-1 kept `shiftLeft`/`shiftRight` here because `phases_late`'s twin had no
// `checkRange` guard, so `x.shiftRight(Integer.MIN_VALUE)` there allocated
// `vec![0u32; 67_108_864]` (~256 MB) before answering. Lane F2 has since landed
// that guard as `phases_late::p71_bi_checked_shl`. Keeping a second copy after
// that would mean TWO live registrations for one triple whose behaviour differs
// only in which lane's guard runs — decided by registration order, i.e. by
// mode. Deleting this copy leaves ONE implementation, reached in every mode.
// `bi_shift_arg` and `bi_checked_shl` went with it; `p71_bi_checked_shl` is
// their surviving twin and applies the identical magnitude-bit rule.

/// `ArithmeticException("BigInteger would overflow supported range")` — JDK 25
/// `BigInteger.reportOverflow`, `BigInteger.java:1220`.
fn bi_overflow() -> MethodCallFailed {
    RuntimeError::ArithmeticException {
        message: "BigInteger would overflow supported range".to_string(),
    }
    .into()
}

/// `BigInteger.pow`'s range guard, ported from JDK 25 `BigInteger.java:2594-2650`.
///
/// `pow` is the second argument-driven allocation in this family (the first is
/// `phases_late::p71_bi_checked_shl`, the third was `BigInt::test_bit`): the exponent alone
/// decides how big the answer is, and this native's square-and-multiply loop
/// happily starts building it. `BigInteger.TEN.pow(1_000_000_000)` is one line
/// of ordinary bytecode asking for a 3.3-billion-bit number. HotSpot does not
/// start: it bounds the result from the operand's bit length and the exponent
/// and throws. MEASURED (`scratchpad/f7/Pow.java`, Microsoft OpenJDK 25.0.3+9):
///
/// ```text
/// TEN.pow(1000000000)   !! ArithmeticException: BigInteger would overflow supported range   [0 ms]
/// THREE.pow(MAX)        !! ArithmeticException: …                                           [0 ms]
/// (2^100).pow(1<<26)    !! ArithmeticException: …                                           [0 ms]
/// TWO.pow(MAX)          !! ArithmeticException: …                                           [94 ms]
/// TWO.pow(MAX-1)         = <signum=1 bitLength=2147483647>                                  [36 ms]
/// THREE.pow(1<<20)       = <signum=1 bitLength=1661954>                                    [335 ms]
/// ```
///
/// Only the REFUSAL is ported; the exponentiation stays the existing
/// square-and-multiply, which is mathematically identical to the JDK's
/// repeated squaring. That keeps the accepted set the same on both sides:
/// the JDK factors `2^powersOfTwo` out of the base and shifts it back at the
/// end, so its guard bounds `(remainingBits - 1)·exponent + bitsToShift + 1`,
/// which is exactly the magnitude bit length of the answer this loop builds.
///
/// Called only with `exponent >= 2` and `|base| >= 2` — the trivial rows are
/// already answered above.
fn bi_pow_check_range(base: &crate::bigint::BigInt, exp: i32) -> Result<(), MethodCallFailed> {
    // `final int powersOfTwo = base.getLowestSetBit();`
    // `final long bitsToShiftLong = (long) powersOfTwo * exponent;`
    // `if (bitsToShift != bitsToShiftLong) reportOverflow();` — both factors
    // are non-negative here, so the narrowing survives iff the product fits.
    let powers_of_two = i64::from(base.lowest_set_bit().max(0));
    let bits_to_shift = powers_of_two * i64::from(exp);
    if bits_to_shift > i64::from(i32::MAX) {
        return Err(bi_overflow());
    }
    // `base = base.shiftRight(powersOfTwo); final int remainingBits = base.bitLength();`
    // Shifting out the trailing zeros removes exactly that many bits, so this
    // needs no shifted copy of the magnitude.
    let remaining_bits = base.magnitude_bits() as i64 - powers_of_two;
    if remaining_bits == 1 {
        // `return (negative ? NEGATIVE_ONE : ONE).shiftLeft(bitsToShift);` —
        // a magnitude of exactly `bitsToShift + 1` bits, refused by
        // `shiftLeft`'s own `checkRange`. This is the `TWO.pow(MAX)` row: it
        // throws, while `TWO.pow(MAX-1)` lands on bitLength 2147483647.
        if 1 + bits_to_shift > i64::from(i32::MAX) {
            return Err(bi_overflow());
        }
        return Ok(());
    }
    // `final long scaleFactor = (long) remainingBits * exponent;`
    // `if (scaleFactor <= Long.SIZE) { …small path, cannot overflow… }`
    // `if (scaleFactor + bitsToShift - exponent >= Integer.MAX_VALUE) reportOverflow();`
    let scale_factor = remaining_bits * i64::from(exp);
    if scale_factor > 64 && scale_factor + bits_to_shift - i64::from(exp) >= i64::from(i32::MAX) {
        return Err(bi_overflow());
    }
    Ok(())
}

/// The `10^n` factor every `BigDecimal` rescale needs.
///
/// `BigDecimal.bigTenToThe(n)` (`BigDecimal.java`) answers small `n` from a
/// table and everything else with `BigInteger.TEN.pow(n)`, so it inherits
/// [`bi_pow_check_range`]'s refusal exactly, and refusing here reproduces the
/// JDK's own control flow rather than inventing a cap. MEASURED
/// (`scratchpad/f7/Scale.java`, Microsoft OpenJDK 25.0.3+9):
///
/// ```text
/// new BigDecimal("1.5").setScale(Integer.MAX_VALUE, HALF_UP)
///     !! ArithmeticException: BigInteger would overflow supported range   [15 ms]
/// new BigDecimal("1.5").setScale(1000000, HALF_UP)  = <1000002 chars, scale=1000000>  [2950 ms]
/// ```
///
/// Both rows come out of the predicate below: `raise = 2147483646` is refused
/// (`3·raise >= Integer.MAX_VALUE`) and `raise = 1000000` is admitted.
fn bd_pow_ten_check(n: i32) -> Result<(), MethodCallFailed> {
    if n < 2 {
        return Ok(());
    }
    bi_pow_check_range(&crate::bigint::BigInt::from_decimal("10"), n)
}

/// Guards against the shape this lane was sent after: a loop or an allocation
/// whose size comes from an ARGUMENT rather than from the operands' magnitude.
///
/// Every expected value below is a line of a `java` transcript on Microsoft
/// OpenJDK 25.0.3+9; the probes are `scratchpad/f7/{Pow,Scale,Shr}.java`.
#[cfg(test)]
mod argument_driven_range_tests {
    use super::{
        bd_narrowing_truncates_to_zero, bd_plain_string_check, bd_pow_ten_check, bd_product_scale,
        bd_rescale_operand, bd_to_big_integer_check, bd_to_f32, bd_to_f64, bd_truncate,
        bi_pow_check_range, bi_shift_right_str,
    };
    use crate::bigint::BigInt;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::{MethodCallFailed, VmError};

    /// `"<ExceptionClass>: <message>"` — the same shape as the `!!` column of
    /// the `java` transcripts these rows were read off, so a row can be
    /// compared to its transcript line by eye.
    fn thrown(e: MethodCallFailed) -> String {
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(r)) => format!("{r}"),
            other => format!("{other}"),
        }
    }

    fn plain(unscaled: &str, scale: i32) -> Result<(), String> {
        bd_plain_string_check(unscaled, scale).map_err(thrown)
    }

    fn to_bigint(unscaled: &str, scale: i32) -> Result<String, String> {
        let u = BigInt::from_decimal(unscaled);
        bd_to_big_integer_check(&u, scale).map_err(thrown)?;
        Ok(bd_truncate(&u, scale).to_decimal())
    }

    /// `BigDecimal.longValue()`, exactly as `native_bd_long_value` composes it.
    fn narrow_long(unscaled: &str, scale: i32) -> i64 {
        let u = BigInt::from_decimal(unscaled);
        if bd_narrowing_truncates_to_zero(&u, scale) {
            return 0;
        }
        crate::bigint_low_twos_complement(&bd_truncate(&u, scale), 64) as i64
    }

    /// `BigDecimal.intValue()`, exactly as `native_bd_int_value` composes it.
    fn narrow_int(unscaled: &str, scale: i32) -> i32 {
        let u = BigInt::from_decimal(unscaled);
        if bd_narrowing_truncates_to_zero(&u, scale) {
            return 0;
        }
        crate::bigint_low_twos_complement(&bd_truncate(&u, scale), 32) as u32 as i32
    }

    /// `a.add(b)`, exactly as `native_bd_add` composes it: `(unscaled, scale)`
    /// of the exact result, or the refusal text.
    fn add_scales(ua: &str, sa: i32, ub: &str, sb: i32) -> Result<(String, i32), String> {
        let (ua, ub) = (BigInt::from_decimal(ua), BigInt::from_decimal(ub));
        let s = sa.max(sb);
        let a = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa)).map_err(thrown)?;
        let b = bd_rescale_operand(&ub, i64::from(s) - i64::from(sb)).map_err(thrown)?;
        Ok((a.add(&b).to_decimal(), s))
    }

    const OVERFLOW_RANGE: &str = "ArithmeticException: BigInteger would overflow supported range";
    const UNDERFLOW: &str = "ArithmeticException: Underflow";
    const OVERFLOW: &str = "ArithmeticException: Overflow";
    const TOO_LARGE: &str = "OutOfMemoryError: too large to fit in a String";

    /// `BigDecimal.toPlainString()` — `scratchpad/f31/{Bd,Bd2,Bd4,Bd5}.java` on
    /// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`. `bd(u,s)` is
    /// `new BigDecimal(BigInteger.valueOf(u), s)`; every row below is a line of
    /// that transcript. See [`super::bd_plain_string_check`] for the full one.
    ///
    /// The three pairs that make this more than a smoke test, because each one
    /// separates two candidate rules that agree everywhere else:
    ///
    /// * `bd(0,MIN)` = `0` but `bd(0,MAX)` OOMEs — the zero short-circuit is on
    ///   the NEGATIVE-scale road only.
    /// * `bd(1,-2147483646)` passes but `bd(12,-2147483646)` and
    ///   `bd(-1,-2147483646)` do not — `len` counts the SIGNED rendering there.
    /// * `bd(1,2147483645)` passes but `bd(-1,2147483645)` does not — `len` on
    ///   the positive road is `(signum<0 ? 3 : 2) + scale`, with no digits in
    ///   it at all.
    #[test]
    fn plain_string_refusals_match_hotspot() {
        // `checkScaleNonZero` CASTS, so only i32::MIN fails, and it lands
        // negative: "Overflow" — the opposite word from `toBigInteger`'s.
        assert_eq!(plain("1", i32::MIN), Err(OVERFLOW.to_string()));
        assert_eq!(plain("-1", i32::MIN), Err(OVERFLOW.to_string()));
        // `if (signum() == 0) return "0";` — negative scales only.
        assert_eq!(plain("0", i32::MIN), Ok(()));
        assert_eq!(plain("0", -5), Ok(()));
        assert_eq!(plain("0", 5), Ok(()));
        assert_eq!(plain("0", i32::MAX), Err(TOO_LARGE.to_string()));
        // Negative road: `len = str.length() + trailingZeros`, `str` signed.
        assert_eq!(plain("1", -2147483647), Err(TOO_LARGE.to_string()));
        assert_eq!(plain("1", -2147483646), Ok(()));
        assert_eq!(plain("1", -2147483645), Ok(()));
        assert_eq!(plain("12", -2147483646), Err(TOO_LARGE.to_string()));
        assert_eq!(plain("-1", -2147483646), Err(TOO_LARGE.to_string()));
        assert_eq!(plain("-1", -2147483645), Ok(()));
        // Positive road: `len = (signum < 0 ? 3 : 2) + scale`.
        assert_eq!(plain("1", 2147483646), Err(TOO_LARGE.to_string()));
        assert_eq!(plain("1", 2147483645), Ok(()));
        assert_eq!(plain("-1", 2147483645), Err(TOO_LARGE.to_string()));
        // Ordinary rows, both signs and both sides of the decimal point.
        assert_eq!(plain("1", 0), Ok(()));
        assert_eq!(plain("123", 3), Ok(()));
        assert_eq!(plain("-123", 4), Ok(()));
        assert_eq!(plain("1", -3), Ok(()));
    }

    /// `BigDecimal.toBigInteger()` — `scratchpad/f31/{Bd,Bd2}.java`.
    ///
    /// The word is `"Underflow"` here and `"Overflow"` in
    /// [`plain_string_refusals_match_hotspot`] for the SAME scale, because
    /// `setScale` reaches the clamping instance `checkScale` and
    /// `toPlainString` reaches the casting static `checkScaleNonZero`. If the
    /// two are ever unified, exactly one of these two tests goes red.
    #[test]
    fn to_big_integer_refusals_match_hotspot() {
        assert_eq!(to_bigint("1", i32::MIN), Err(UNDERFLOW.to_string()));
        assert_eq!(to_bigint("-1", i32::MIN), Err(UNDERFLOW.to_string()));
        assert_eq!(
            to_bigint("1", i32::MIN + 1),
            Err(OVERFLOW_RANGE.to_string())
        );
        assert_eq!(
            to_bigint("1", -715_827_883),
            Err(OVERFLOW_RANGE.to_string())
        );
        // Positive scales refuse too: `setScale(0)` builds `bigTenToThe(drop)`.
        assert_eq!(to_bigint("1", i32::MAX), Err(OVERFLOW_RANGE.to_string()));
        assert_eq!(to_bigint("1", 715_827_883), Err(OVERFLOW_RANGE.to_string()));
        // `zero can have any scale` — `setScale`'s signum test comes first.
        assert_eq!(to_bigint("0", i32::MIN), Ok("0".to_string()));
        assert_eq!(to_bigint("0", i32::MAX), Ok("0".to_string()));
        assert_eq!(to_bigint("0", 715_827_883), Ok("0".to_string()));
        // Values.
        assert_eq!(to_bigint("1", -3), Ok("1000".to_string()));
        assert_eq!(to_bigint("123456", 5), Ok("1".to_string()));
        assert_eq!(to_bigint("-123456", 5), Ok("-1".to_string()));
        assert_eq!(to_bigint("1", 0), Ok("1".to_string()));
    }

    /// `BigDecimal.intValue()` / `longValue()` — `scratchpad/f31/{Bd,Bd4}.java`.
    ///
    /// Every row here shares its `(unscaled, scale)` with a row of
    /// [`to_big_integer_refusals_match_hotspot`] that REFUSES. That is the
    /// whole point of the split: HotSpot answers `0` in under a millisecond for
    /// `bd(1,MIN)` and `bd(1,MAX)`, so a guard on the shared truncation helper
    /// would have been a fresh divergence, and this file's previous body
    /// answered `1` for `bd(1,MIN).intValue()` in release (it negated
    /// `i32::MIN`, wrapped, and took `bigint_mul_pow10`'s `n <= 0` arm).
    #[test]
    fn narrowing_conversions_match_hotspot() {
        // The three fast paths, at their exact boundaries.
        assert_eq!(narrow_long("1", i32::MIN), 0);
        assert_eq!(narrow_long("0", i32::MIN), 0);
        assert_eq!(narrow_long("1", i32::MAX), 0);
        assert_eq!(narrow_long("1", 715_827_883), 0);
        assert_eq!(narrow_long("1", -65), 0);
        assert_eq!(narrow_long("1", -64), 0);
        assert_eq!(narrow_long("7", -64), 0);
        // `scale <= -64` is exact, not conservative: one less is NOT zero.
        assert_eq!(narrow_long("1", -63), i64::MIN);
        assert_eq!(narrow_long("7", -63), i64::MIN);
        // `fractionOnly()` — `precision() <= scale`.
        assert_eq!(narrow_long("1", 5), 0);
        assert_eq!(narrow_long("123456", 5), 1);
        assert_eq!(narrow_long("123456", 6), 0);
        assert_eq!(narrow_long("123456", 7), 0);
        assert_eq!(narrow_long("-123456", 5), -1);
        // The upper-bound `precision()` does NOT fire at 31 digits/scale 31
        // (`bits/3 + 1` is 34 there), so this row proves the fall-through
        // division reaches HotSpot's answer anyway.
        let ten_pow_30 = format!("1{}", "0".repeat(30));
        assert_eq!(narrow_long(&ten_pow_30, 30), 1);
        assert_eq!(narrow_long(&ten_pow_30, 31), 0);
        assert_eq!(narrow_long(&ten_pow_30, -2), -8_814_407_033_341_083_648);
        // Ordinary negative scales.
        assert_eq!(narrow_long("1", -1), 10);
        assert_eq!(narrow_long("-7", -3), -7000);
        assert_eq!(narrow_long("3", -20), 4_852_094_820_647_174_144);
        // intValue is `(int) longValue()`, i.e. the low 32 bits of the same.
        assert_eq!(narrow_int("1", i32::MIN), 0);
        assert_eq!(narrow_int("1", i32::MAX), 0);
        assert_eq!(narrow_int("1", -63), 0);
        assert_eq!(narrow_int("1", -33), 0);
        assert_eq!(narrow_int("1", -32), 0);
        assert_eq!(narrow_int("3", -20), 691_011_584);
        assert_eq!(narrow_int("-123456", 5), -1);
    }

    /// `BigDecimal.add`/`subtract` scale alignment —
    /// `scratchpad/f31/{Bd,Bd2,Bd4}.java`.
    ///
    /// Rows 1-6 all used to compute `s - sa` in `i32`. `i32::MAX - i32::MIN`
    /// panics in debug and wraps to `-1` in release, and `-1` is
    /// `bigint_mul_pow10`'s "nothing to do" arm, so the release build returned
    /// the operand UNSCALED — a wrong answer, silently.
    ///
    /// Rows 7-13 are the reason the guard tests the RAISED OPERAND and not the
    /// raise: `("1",MIN)+("0",MAX)` refuses and `("0",MIN)+("1",MAX)` answers,
    /// and they differ in nothing but which operand is the zero.
    #[test]
    fn add_scale_alignment_matches_hotspot() {
        assert_eq!(
            add_scales("1", i32::MIN, "1", i32::MAX),
            Err(UNDERFLOW.into())
        );
        assert_eq!(
            add_scales("1", i32::MAX, "1", i32::MIN),
            Err(UNDERFLOW.into())
        );
        assert_eq!(
            add_scales("1", i32::MIN, "0", i32::MAX),
            Err(UNDERFLOW.into())
        );
        assert_eq!(add_scales("0", 0, "1", i32::MIN), Err(UNDERFLOW.into()));
        assert_eq!(
            add_scales("0", i32::MAX, "1", i32::MIN),
            Err(UNDERFLOW.into())
        );
        assert_eq!(
            add_scales("1", 0, "1", 715_827_883),
            Err(OVERFLOW_RANGE.into())
        );
        assert_eq!(
            add_scales("1", 0, "0", 715_827_883),
            Err(OVERFLOW_RANGE.into())
        );
        assert_eq!(
            add_scales("0", 715_827_883, "1", 0),
            Err(OVERFLOW_RANGE.into())
        );
        // A ZERO raised operand is exempt from BOTH guards, and is not built.
        assert_eq!(
            add_scales("0", i32::MIN, "1", i32::MAX),
            Ok(("1".to_string(), i32::MAX))
        );
        assert_eq!(
            add_scales("0", 0, "1", i32::MAX),
            Ok(("1".to_string(), i32::MAX))
        );
        assert_eq!(
            add_scales("1", i32::MAX, "0", 0),
            Ok(("1".to_string(), i32::MAX))
        );
        assert_eq!(
            add_scales("0", 0, "1", 715_827_883),
            Ok(("1".to_string(), 715_827_883))
        );
        // Ordinary alignment: 0.01 + 0.3 = 0.31, and 1000 + 0.3 = 1000.3.
        assert_eq!(add_scales("1", 2, "3", 1), Ok(("31".to_string(), 2)));
        assert_eq!(add_scales("1", -3, "3", 1), Ok(("10003".to_string(), 1)));
    }

    fn refused(v: &str, exp: i32) -> bool {
        bi_pow_check_range(&BigInt::from_decimal(v), exp).is_err()
    }

    /// `scratchpad/f7/Pow.java`:
    ///
    /// ```text
    /// TWO.pow(MAX)       !! ArithmeticException: BigInteger would overflow supported range [94 ms]
    /// TWO.pow(MAX-1)      = <signum=1 bitLength=2147483647>                                [36 ms]
    /// TWO.pow(1<<30)      = <signum=1 bitLength=1073741825>                                [50 ms]
    /// THREE.pow(MAX)     !! …overflow…  [0 ms]      THREE.pow(1<<20) = <bitLength=1661954> [335 ms]
    /// TEN.pow(MAX)       !! …overflow…  [0 ms]      TEN.pow(1000000000) !! …overflow…      [0 ms]
    /// (2^100).pow(MAX)   !! …overflow…  [0 ms]      (2^100).pow(1<<26)  !! …overflow…      [0 ms]
    /// (-2).pow(MAX)      !! …overflow…  [68 ms]     (2^31-1).pow(MAX)   !! …overflow…      [0 ms]
    /// TEN.pow(3) = 1000     (-2).pow(3) = -8     (-2).pow(4) = 16
    /// ```
    ///
    /// A further 2,560 (base, exponent) rows agree on class, message and value
    /// (`scratchpad/f7/PowParity.java`: `POW PARITY cases=2560 diffs=0
    /// value-compared=1635`).
    #[test]
    fn pow_range_guard_matches_hotspot() {
        assert!(refused("2", i32::MAX));
        assert!(refused("-2", i32::MAX));
        assert!(refused("3", i32::MAX));
        assert!(refused("-3", i32::MAX));
        assert!(refused("10", i32::MAX));
        assert!(refused("10", 1_000_000_000));
        assert!(refused("2147483647", i32::MAX));
        // 2^100: refused by the `powersOfTwo * exponent` narrowing, not by the
        // scale factor — the two arms are separate rows of the JDK's guard.
        let p100 = BigInt::from_decimal("1267650600228229401496703205376");
        assert!(bi_pow_check_range(&p100, i32::MAX).is_err());
        assert!(bi_pow_check_range(&p100, 1 << 26).is_err());

        assert!(!refused("2", i32::MAX - 1));
        assert!(!refused("2", 1 << 30));
        assert!(!refused("3", 1 << 20));
        assert!(!refused("10", 3));
        assert!(!refused("-2", 3));
        assert!(!refused("-2", 4));
    }

    /// `scratchpad/f7/Scale.java`:
    ///
    /// ```text
    /// 1.5.setScale(MAX_VALUE, HALF_UP) !! ArithmeticException: BigInteger would overflow supported range [15 ms]
    /// 1.5.setScale(1000000)             = <1000002 chars, scale=1000000>                                [2950 ms]
    /// ```
    #[test]
    fn set_scale_pow_ten_guard_matches_hotspot() {
        // `1.5` has scale 1, so `setScale(Integer.MAX_VALUE)` raises by MAX-1.
        assert!(bd_pow_ten_check(i32::MAX - 1).is_err());
        assert!(bd_pow_ten_check(i32::MAX).is_err());
        assert!(bd_pow_ten_check(1_000_000).is_ok());
        // Every scale a real caller uses.
        assert!(bd_pow_ten_check(0).is_ok());
        assert!(bd_pow_ten_check(1).is_ok());
        assert!(bd_pow_ten_check(18).is_ok());
        // The boundary itself: `TEN.pow(n)` refuses once `3n >= Integer.MAX_VALUE`.
        assert!(bd_pow_ten_check(715_827_882).is_ok());
        assert!(bd_pow_ten_check(715_827_883).is_err());
    }

    /// `scratchpad/f7/Shr.java`:
    ///
    /// ```text
    /// (-1).shiftRight(MAX) = -1      (-9).shiftRight(MAX) = -1     (0).shiftRight(MAX) = 0
    /// (-1).shiftRight(100) = -1      (-9).shiftRight(1)   = -5     (9).shiftRight(1)   = 4
    /// (2^100).shiftRight(MAX) = 0    (-2^100).shiftRight(MAX) = -1
    /// ```
    ///
    /// The `q == "0"` guard in the loop only ever fires for a NON-NEGATIVE
    /// value: the negative arm computes `ceildiv(q, 2)`, whose fixpoint is 1.
    /// Each row below used to run the full `n` iterations — 2^31 decimal
    /// divisions — to return a value that had stopped changing after four.
    #[test]
    fn shift_right_str_negative_stops_at_the_fixpoint() {
        assert_eq!(bi_shift_right_str("-1", i32::MAX), "-1");
        assert_eq!(bi_shift_right_str("-9", i32::MAX), "-1");
        assert_eq!(
            bi_shift_right_str("1267650600228229401496703205376", i32::MAX),
            "0"
        );
        assert_eq!(
            bi_shift_right_str("-1267650600228229401496703205376", i32::MAX),
            "-1"
        );
        assert_eq!(bi_shift_right_str("-1", 100), "-1");
        assert_eq!(bi_shift_right_str("-9", 1), "-5");
        assert_eq!(bi_shift_right_str("9", 1), "4");
        assert_eq!(bi_shift_right_str("0", i32::MAX), "0");
    }

    // -----------------------------------------------------------------
    // G10 (2026-08-16) — `multiply`'s product scale, `doubleValue`, and the
    // no-panic pin over every rescale path. Every expectation below is a
    // transcript line from `scratchpad/g10/{BdProbe,Bd2Probe}.java` on Temurin
    // OpenJDK 25.0.3+9, not a derivation from this implementation.
    // -----------------------------------------------------------------

    /// `a.multiply(b)`'s product scale, exactly as `native_bd_multiply`
    /// composes it: the RECEIVER's unscaled value decides the refusal.
    fn product_scale(ua: &str, sa: i32, sb: i32) -> Result<i32, String> {
        bd_product_scale(&BigInt::from_decimal(ua), sa, sb).map_err(thrown)
    }

    /// ```text
    /// bd(1,MAX).multiply(bd(1,MAX))  !! ArithmeticException: Underflow
    /// bd(0,MAX).multiply(bd(1,MAX))   = unscaled=0 scale=2147483647
    /// bd(1,MIN).multiply(bd(1,MIN))  !! ArithmeticException: Overflow
    /// bd(0,MIN).multiply(bd(1,MIN))   = unscaled=0 scale=-2147483648
    /// bd(1,MAX).multiply(bd(1,MIN))   = unscaled=1 scale=-1
    /// bd(1,MAX).multiply(bd(1,0))     = unscaled=1 scale=2147483647
    /// ```
    #[test]
    fn multiply_product_scale_matches_hotspot() {
        // Non-zero receiver, sum past Integer.MAX_VALUE -> "Underflow".
        assert_eq!(
            product_scale("1", i32::MAX, i32::MAX),
            Err("ArithmeticException: Underflow".to_string())
        );
        assert_eq!(
            product_scale("1", i32::MAX, 1),
            Err("ArithmeticException: Underflow".to_string())
        );
        assert_eq!(
            product_scale("1", 1_073_741_824, 1_073_741_824),
            Err("ArithmeticException: Underflow".to_string())
        );
        // Non-zero receiver, sum past Integer.MIN_VALUE -> "Overflow".
        // The two words are NOT interchangeable.
        assert_eq!(
            product_scale("1", i32::MIN, i32::MIN),
            Err("ArithmeticException: Overflow".to_string())
        );
        assert_eq!(
            product_scale("1", i32::MIN, -1),
            Err("ArithmeticException: Overflow".to_string())
        );
        assert_eq!(
            product_scale("1", -1_073_741_824, -1_073_741_825),
            Err("ArithmeticException: Overflow".to_string())
        );
        // A ZERO receiver clamps and answers -- and it is the RECEIVER's
        // zeroness, not the product's: `bd(1,MAX).multiply(bd(0,MAX))` throws.
        assert_eq!(product_scale("0", i32::MAX, i32::MAX), Ok(i32::MAX));
        assert_eq!(
            product_scale("0", 1_073_741_824, 1_073_741_824),
            Ok(i32::MAX)
        );
        assert_eq!(product_scale("0", i32::MIN, i32::MIN), Ok(i32::MIN));
        assert_eq!(product_scale("0", i32::MIN, -1), Ok(i32::MIN));
        // In-range sums are just the sum, at both extremes.
        assert_eq!(product_scale("1", i32::MAX, 0), Ok(i32::MAX));
        assert_eq!(product_scale("1", i32::MIN, 0), Ok(i32::MIN));
        assert_eq!(product_scale("1", i32::MAX, i32::MIN), Ok(-1));
        assert_eq!(product_scale("1", 2, 3), Ok(5));
        assert_eq!(product_scale("-1", -2, -3), Ok(-5));
    }

    /// `scratchpad/g10/Bd2Probe.java`. Every row is `bd(u,s).doubleValue()` on
    /// Temurin OpenJDK 25.0.3+9, and every one of them answers in 0 ms without
    /// HotSpot rendering anything — which is the point: the old body rendered
    /// `scale.unsigned_abs()` zeros first.
    #[test]
    fn double_value_matches_hotspot() {
        let d = |u: &str, s: i32| bd_to_f64(&BigInt::from_decimal(u), s);
        // The scale extremes, on both signs and on zero.
        assert_eq!(d("1", i32::MIN), f64::INFINITY);
        assert_eq!(d("-1", i32::MIN), f64::NEG_INFINITY);
        assert_eq!(d("1", i32::MIN + 1), f64::INFINITY);
        assert_eq!(d("1", i32::MAX), 0.0);
        assert_eq!(d("-1", i32::MAX), -0.0);
        assert!(d("-1", i32::MAX).is_sign_negative());
        assert_eq!(d("0", i32::MIN), 0.0);
        assert_eq!(d("0", i32::MAX), 0.0);
        // A zero value is +0.0 at EVERY scale -- never -0.0.
        assert!(d("0", i32::MIN).is_sign_positive());
        assert!(d("0", 324).is_sign_positive());
        // The overflow boundary, one scale apart.
        assert_eq!(d("1", -308), 1.0e308);
        assert_eq!(d("1", -309), f64::INFINITY);
        assert_eq!(d("15", -308), f64::INFINITY);
        assert_eq!(d("17976931348623157", -292), 1.7976931348623157e308);
        assert_eq!(d("17976931348623159", -292), f64::INFINITY);
        // The underflow boundary. `bd(1,324)` rounds to zero but `bd(15,324)`
        // does not, and they share an adjusted exponent -- which is why the
        // clamp is at -325 and not -324.
        assert_eq!(d("1", 323), 9.9e-324);
        assert_eq!(d("1", 324), 0.0);
        assert_eq!(d("15", 324), 1.5e-323);
        assert_eq!(d("49", 326), 0.0);
        assert_eq!(d("1", 325), 0.0);
        assert_eq!(d("-1", 324), -0.0);
        assert!(d("-1", 324).is_sign_negative());
        // Ordinary values, and the round-to-nearest-even row that a truncating
        // implementation gets wrong.
        assert_eq!(d("1", 0), 1.0);
        assert_eq!(d("1", 1), 0.1);
        assert_eq!(d("1", -1), 10.0);
        assert_eq!(d("15", 1), 1.5);
        assert_eq!(d("-15", -1), -150.0);
        assert_eq!(d("9007199254740993", 0), 9.007199254740992e15);
        assert_eq!(d("9007199254740993", 324), 9.007199254740994e-309);
    }

    /// `floatValue()` is `Float.parseFloat(toString())`, NOT `(float)
    /// doubleValue()` -- the clamps are `f32`'s and the rounding is single.
    #[test]
    fn float_value_matches_hotspot() {
        let f = |u: &str, s: i32| bd_to_f32(&BigInt::from_decimal(u), s);
        assert_eq!(f("1", i32::MIN), f32::INFINITY);
        assert_eq!(f("-1", i32::MIN), f32::NEG_INFINITY);
        assert_eq!(f("1", i32::MAX), 0.0);
        assert!(f("-1", i32::MAX).is_sign_negative());
        assert_eq!(f("0", i32::MIN), 0.0);
        // `bd(1,-308)` is 1.0E308 as a double and Infinity as a float.
        assert_eq!(f("1", -308), f32::INFINITY);
        assert_eq!(f("1", 308), 0.0);
        assert!(f("-1", 308).is_sign_negative());
        assert_eq!(f("1", 1), 0.1f32);
        assert_eq!(f("15", 1), 1.5f32);
        assert_eq!(f("-15", -1), -150.0f32);
        assert_eq!(f("9007199254740993", 1), 9.0071994e14f32);
    }

    /// **The no-panic pin.** A Rust panic in a native is not a Java throwable:
    /// it is not catchable and it takes the VM down where HotSpot throws. Every
    /// rescale path in this file is exercised here at `Integer.MIN_VALUE`,
    /// `Integer.MIN_VALUE + 1`, `Integer.MAX_VALUE` and `0` — the four scales
    /// where a `-scale` / `sa + sb` / `s - sa` spelling overflows — on a zero
    /// operand and a non-zero one. The assertion is not on the VALUE: it is
    /// that each call RETURNS, either an answer or a `MethodCallFailed`.
    ///
    /// These are the four shapes that have each been a live abort in this
    /// family at some point: `apply_scale`'s `-scale` (F20), `bd_set_scale_impl`'s
    /// `new_scale - scale` (F31), `bd_rescale_operand`'s `s - sa` (F31), and
    /// `native_bd_multiply`'s `sa + sb` (G10).
    #[test]
    fn every_rescale_path_is_total_at_the_scale_extremes() {
        let scales = [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
        let operands = ["0", "1", "-1", "15", "-9007199254740993"];
        for &s in &scales {
            for u in &operands {
                let v = BigInt::from_decimal(u);
                // toBigInteger()'s guard, then its arithmetic. `bd_truncate`
                // is only reached when the guard admits, exactly as
                // `bd_truncated_bigint` composes them.
                if bd_to_big_integer_check(&v, s).is_ok() {
                    let _ = bd_truncate(&v, s).to_decimal();
                }
                // intValue()/longValue(): no guard at all, by design.
                let _ = narrow_int(u, s);
                let _ = narrow_long(u, s);
                // toPlainString()'s guard.
                let _ = bd_plain_string_check(u, s);
                // doubleValue()/floatValue(): total, no guard needed.
                let _ = bd_to_f64(&v, s);
                let _ = bd_to_f32(&v, s);
                for &s2 in &scales {
                    // multiply()'s product scale.
                    let _ = bd_product_scale(&v, s, s2);
                    // add()/subtract()'s per-operand raise. The `i64`
                    // subtraction is the caller's job and is spelled here the
                    // way `native_bd_add` spells it.
                    let common = s.max(s2);
                    let _ = bd_rescale_operand(&v, i64::from(common) - i64::from(s));
                    let _ = bd_rescale_operand(&v, i64::from(common) - i64::from(s2));
                }
            }
        }
    }

    /// `bi_mod_pow_str` used to `panic!` on a negative exponent. MEASURED on
    /// Temurin OpenJDK 25.0.3+9 (`scratchpad/g10/BiProbe.java`):
    ///
    /// ```text
    /// 3.modPow(-1, 7)  = 5      3.modPow(2, 1) = 0      (-3).modPow(3, 7) = 1
    /// 2.modPow(-1, 4) !! ArithmeticException: BigInteger not invertible.
    /// ```
    #[test]
    fn mod_pow_negative_exponent_no_longer_panics() {
        use super::{bi_mod_pow_str, bi_mod_pow_str_opt};
        // The invertible case is now COMPUTED, not aborted.
        assert_eq!(bi_mod_pow_str_opt("3", "-1", "7").as_deref(), Some("5"));
        assert_eq!(bi_mod_pow_str_opt("2", "-1", "7").as_deref(), Some("4"));
        assert_eq!(bi_mod_pow_str_opt("3", "-2", "10").as_deref(), Some("9"));
        assert_eq!(bi_mod_pow_str_opt("2", "-3", "7").as_deref(), Some("1"));
        // The one shape a `-> String` cannot express.
        assert_eq!(bi_mod_pow_str_opt("2", "-1", "4"), None);
        assert_eq!(bi_mod_pow_str_opt("2", "-1", "8"), None);
        assert_eq!(bi_mod_pow_str_opt("0", "-5", "7"), None);
        // ...and the infallible wrapper RETURNS there rather than aborting the
        // VM. `"0"` is a wrong value, deliberately preferred to a panic; no
        // in-tree caller can reach it.
        assert_eq!(bi_mod_pow_str("2", "-1", "4"), "0");
        assert_eq!(bi_mod_pow_str("0", "-5", "7"), "0");
        // The non-negative rows are unchanged.
        assert_eq!(bi_mod_pow_str("3", "4", "7"), "4");
        assert_eq!(bi_mod_pow_str("-3", "3", "7"), "1");
        assert_eq!(bi_mod_pow_str("3", "2", "1"), "0");
    }
}

// ---------------------------------------------------------------------------
// BigInteger(String) / BigInteger(String, int)

/// `digitsPerInt` from JDK 25 `BigInteger.java:4795-4797`, verbatim. It exists
/// here only to reproduce the exception MESSAGE: the real constructor parses in
/// groups of this many digits with `Integer.parseInt(group, radix)`, so a bad
/// character is reported as `For input string: "<the group it fell in>"` and
/// not as the whole operand. Index 0/1 are unused (radix is 2..=36).
const BI_DIGITS_PER_INT: [usize; 37] = [
    0, 0, 30, 19, 15, 13, 11, 11, 10, 9, 9, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 5,
];

/// The zero of every `Nd` (decimal digit) block of the Basic Multilingual
/// Plane, ascending. Unicode 16.0.0 -- the version JDK 25's `Character` data
/// is built from -- generated from the UCD (Python 3.14 `unicodedata`,
/// `unidata_version` 16.0.0: every BMP code point with category `Nd` and
/// decimal value 0; each block is exactly ten consecutive `Nd` code points
/// valued 0..9, 370 in all). Supplementary digits are absent on purpose: the
/// JDK parses `BigInteger` text one UTF-16 `char` at a time, and a surrogate
/// is never a digit. Entry for entry the same blocks as `lang_math.rs`
/// `JAVA_DIGIT_RUNS` (the `Integer.parseInt` natives' table, generated from
/// JDK 25 itself), which is private to that file; a shared helper is
/// proposal CP2-4.
const BMP_ND_ZEROS: [u16; 37] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66,
    0x0CE6, 0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946,
    0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
    0xA9F0, 0xAA50, 0xABF0, 0xFF10,
];

/// `CRATONVM_BIGINT_UNICODE_DIGITS` (default on; `0` restores the ASCII-only
/// digits): round 14 wave 2, lane compat2. Read only when a non-ASCII UTF-16
/// unit reaches [`bi_java_digit`], so the ASCII constructor never pays for it.
fn bi_unicode_digits_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGINT_UNICODE_DIGITS")
}

/// `Character.digit(char, radix)` for one UTF-16 unit, as the real
/// `BigInteger(String, int)` and `Integer.parseInt` apply it (JDK 25
/// `CharacterData.digit`): a `DECIMAL_DIGIT_NUMBER` is its decimal value, the
/// "Java supradecimal" letters `A-Z`, `a-z` and their fullwidth forms
/// U+FF21..FF3A / U+FF41..FF5A are 10..35, and the value must be below
/// `radix`. So HotSpot reads `new BigInteger("\u{663}")` as 3 and
/// `new BigInteger("\u{FF11}\u{FF10}", 16)` as 16. Other numerics (`Nl`,
/// `No`: Roman numerals, vulgar fractions, superscripts) are not digits.
fn bi_java_digit(c: u16, radix: u32) -> Option<u32> {
    let v = match c {
        0x30..=0x39 => u32::from(c - 0x30),
        0x41..=0x5A => u32::from(c - 0x41) + 10,
        0x61..=0x7A => u32::from(c - 0x61) + 10,
        0..=0x7F => return None,
        _ if !bi_unicode_digits_enabled() => return None,
        0xFF21..=0xFF3A => u32::from(c - 0xFF21) + 10,
        0xFF41..=0xFF5A => u32::from(c - 0xFF41) + 10,
        _ => {
            let zero = BMP_ND_ZEROS[BMP_ND_ZEROS.partition_point(|&z| z <= c).checked_sub(1)?];
            let d = u32::from(c - zero);
            if d >= 10 {
                return None;
            }
            d
        }
    };
    (v < radix).then_some(v)
}

/// Reproduce `Integer.parseInt(group, radix)`'s message for the group that
/// contains the first non-digit, matching the real constructor's grouping.
///
/// The JDK skips leading zeros first, then splits the REMAINING digits into a
/// short first group of `numDigits % digitsPerInt[radix]` (or a full group when
/// that is 0) followed by full groups. Confirmed against HotSpot on four
/// independent shapes: `"1_0"` -> `"1_0"`, `" 7"` -> `" 7"`,
/// `"1234567890123_4567890"` -> `"3_4567890"`, and
/// `"aaaaaaaaaaaaaaaaaaaaaaaaG"` radix 16 -> `"aaaaaaG"`.
fn bi_number_format_message(digits: &[u16], bad_at: usize, radix: u32) -> String {
    let per = BI_DIGITS_PER_INT[radix as usize];
    // Leading zeros are consumed before grouping starts.
    let zeros = digits
        .iter()
        .take_while(|c| bi_java_digit(**c, radix) == Some(0))
        .count();
    let group = if bad_at < zeros || per == 0 {
        // The bad character is inside the leading-zero run (so the run stopped
        // there and grouping starts at it), or an impossible radix.
        String::from_utf16_lossy(&digits[bad_at..])
    } else {
        let n_digits = digits.len() - zeros;
        let first = match n_digits % per {
            0 => per,
            r => r,
        };
        let off = bad_at - zeros;
        let (start, len) = if off < first {
            (zeros, first)
        } else {
            let g = (off - first) / per;
            (zeros + first + g * per, per)
        };
        let end = (start + len).min(digits.len());
        String::from_utf16_lossy(&digits[start..end])
    };
    if radix == 10 {
        format!("For input string: \"{group}\"")
    } else {
        format!("For input string: \"{group}\" under radix {radix}")
    }
}

/// The whole of `java.math.BigInteger(String val, int radix)`'s validation, in
/// the JDK's own order (JDK 25 `BigInteger.java:526-602`), returning the
/// canonical signed decimal string this file stores.
///
/// Order matters and was verified one row at a time on HotSpot 25.0.3+9
/// (`scratchpad/e38/Ctor.java`):
///
/// ```text
/// new BigInteger("", 1)   !! NumberFormatException: Radix out of range     (radix beats length)
/// new BigInteger("")      !! NumberFormatException: Zero length BigInteger
/// new BigInteger("-")     !! NumberFormatException: Zero length BigInteger (sign-only)
/// new BigInteger("5-")    !! NumberFormatException: Illegal embedded sign character
/// new BigInteger("--5")   !! NumberFormatException: Illegal embedded sign character
/// new BigInteger("-+5")   !! NumberFormatException: Illegal embedded sign character
/// new BigInteger("+7")     = 7        new BigInteger("-000") = 0
/// new BigInteger("1_0")   !! NumberFormatException: For input string: "1_0"
/// ```
///
/// The sign rule is `lastIndexOf`, not "starts with": that is why `"5-"` and
/// `"1+2"` are *sign* errors rather than digit errors.
///
/// NULL is handled by the callers, before this: `val.length()` is the real
/// constructor's first statement, so `new BigInteger(null, 40)` is a
/// `NullPointerException` and NOT `Radix out of range` (measured).
fn bi_parse_java(s: &str, radix: i32) -> Result<String, MethodCallFailed> {
    if !(2..=36).contains(&radix) {
        return Err(RuntimeError::NumberFormatException {
            message: "Radix out of range".to_string(),
        }
        .into());
    }
    let nfe = |message: String| -> MethodCallFailed {
        RuntimeError::NumberFormatException { message }.into()
    };
    if s.is_empty() {
        return Err(nfe("Zero length BigInteger".to_string()));
    }
    // "Check for at most one leading sign" — `val.lastIndexOf`. '-' and '+' are
    // ASCII, so a byte index of 0 is a char index of 0.
    let last_minus = s.rfind('-');
    let last_plus = s.rfind('+');
    let mut neg = false;
    let mut rest = s;
    if let Some(i) = last_minus {
        if i != 0 || last_plus.is_some() {
            return Err(nfe("Illegal embedded sign character".to_string()));
        }
        neg = true;
        rest = &s[1..];
    } else if let Some(i) = last_plus {
        if i != 0 {
            return Err(nfe("Illegal embedded sign character".to_string()));
        }
        rest = &s[1..];
    }
    if rest.is_empty() {
        return Err(nfe("Zero length BigInteger".to_string()));
    }
    // UTF-16 units, as the JDK indexes them: the leading-zero run, the digit
    // groups of the message, and a surrogate (never a digit) all count in
    // `char`s, not code points.
    let digits: Vec<u16> = rest.encode_utf16().collect();
    let radix_u = radix as u32;
    if let Some(bad) = digits
        .iter()
        .position(|c| bi_java_digit(*c, radix_u).is_none())
    {
        return Err(nfe(bi_number_format_message(&digits, bad, radix_u)));
    }
    if radix == 10 {
        // Already decimal — no limb round trip for the hot constructor. Strip
        // leading zeros so the stored string is canonical ("-000" is 0, and
        // `bi_write_into` derives `signum` from the leading '-'). Non-ASCII
        // digits (every one validated above) are rewritten to ASCII first.
        let ascii: String;
        let rest = if rest.is_ascii() {
            rest
        } else {
            ascii = digits
                .iter()
                .map(|&c| char::from(b'0' + bi_java_digit(c, 10).unwrap_or(0) as u8))
                .collect();
            ascii.as_str()
        };
        let trimmed = rest.trim_start_matches('0');
        if trimmed.is_empty() {
            return Ok("0".to_string());
        }
        return Ok(if neg {
            format!("-{trimmed}")
        } else {
            trimmed.to_string()
        });
    }
    let base = crate::bigint::BigInt::from_le_words(false, vec![radix_u]);
    let mut acc = crate::bigint::BigInt::zero();
    for &ch in &digits {
        // `bi_java_digit` already answered `Some` for every char above.
        let d = bi_java_digit(ch, radix_u).unwrap_or(0);
        acc = acc
            .mul(&base)
            .add(&crate::bigint::BigInt::from_le_words(false, vec![d]));
    }
    if acc.is_zero() {
        return Ok("0".to_string());
    }
    Ok(if neg {
        acc.neg_value().to_decimal()
    } else {
        acc.to_decimal()
    })
}

/// `new BigInteger(String)` — the real constructor is `this(val, 10)`
/// (JDK 25 `BigInteger.java:620`-ish), so it delegates.
///
/// This validated NOTHING before: `new BigInteger("abc")` wrote "abc" into the
/// value slot with `signum = 1`, `new BigInteger("")` produced a non-zero
/// BigInteger over the empty string, and `new BigInteger("+7")` stored "+7"
/// (whose magnitude words are whatever `decimal_to_mag_words` makes of a '+').
fn native_bi_init_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let s = bi_ctor_string_arg(ctx, args.get(1))?;
    let decimal = bi_parse_java(&s, 10)?;
    bi_write_into(ctx, this, &decimal);
    Ok(None)
}

/// The `String val` argument of either constructor. A null is a
/// `NullPointerException` from `val.length()` — the real constructor's first
/// statement, ahead of even the radix check (measured: `new BigInteger(null,
/// 40)` is an NPE, not `Radix out of range`).
fn bi_ctor_string_arg(
    ctx: &dyn NativeContext,
    arg: Option<&Value>,
) -> Result<String, MethodCallFailed> {
    match arg {
        Some(Value::Object(Some(o))) => Ok(ctx.read_string(*o).unwrap_or_default()),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("Cannot invoke \"String.length()\" because \"val\" is null".to_string()),
        }
        .into()),
    }
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
    // gc-common w20-c: either allocation below can collect and move `this`,
    // which is written after it; re-read it from a pin.
    let this_pin = ctx.pin_native_root(this);
    if let Some((sig_i, mag_i)) = bi_layout(ctx) {
        let mag_words = decimal_to_mag_words(value);
        let mag_arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, mag_words.len());
        for (i, w) in mag_words.iter().enumerate() {
            ctx.set_array_element(mag_arr, i, Value::Int(*w as i32));
        }
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, sig_i, Value::Int(signum));
        ctx.set_field(this, mag_i, Value::Object(Some(mag_arr)));
    } else {
        // Uninterned, like `bi_alloc`'s synthetic arm.
        let val_str = ctx.create_string_uninterned(value);
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, BI_FIELD_VALUE, Value::Object(Some(val_str)));
        ctx.set_field(this, BI_FIELD_SIGNUM, Value::Int(signum));
    }
    ctx.unpin_native_roots(this_pin);
}

/// `new BigInteger(String, int)`.
///
/// Unlike `toString(int)` — which IGNORES a bad radix and uses 10 — this
/// CONSTRUCTOR throws. Measured on real JDK 25: radix 0, 1, -1, 37, 40 and
/// `Integer.MIN_VALUE` all raise `NumberFormatException: Radix out of range`.
/// That guard also closes a panic that predates it: an older body called
/// `i128::from_str_radix(abs, radix as u32)`, and `from_str_radix` PANICS for a
/// radix outside 2..=36 (a negative radix widening to a huge `u32` besides), so
/// an ordinary `new BigInteger(s, 40)` from Java aborted the VM instead of
/// throwing.
///
/// What it still did not do was validate the DIGITS on the radix-10 path — the
/// path `new BigInteger(String)` also lands on — where the operand was passed
/// through as if it were already a decimal literal. See [`bi_parse_java`].
fn native_bi_init_string_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let s = bi_ctor_string_arg(ctx, args.get(1))?;
    let radix = match args.get(2) {
        Some(Value::Int(r)) => *r,
        _ => 10,
    };
    let decimal = bi_parse_java(&s, radix)?;
    bi_write_into(ctx, this, &decimal);
    Ok(None)
}

fn native_bi_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(l)) => *l,
        _ => 0,
    };
    // Round 13 wave 9 (lane bigdec2, `CRATONVM_BIGNUM_JDK_IDENTITY`): the
    // JDK's own answers for -16..16 -- `ZERO`, `posConst[val]`,
    // `negConst[-val]` (JDK 25 `BigInteger.valueOf`, BigInteger.java:1236).
    // The spec allows fresh instances, but HotSpot answers these constants and
    // `pow`'s `return ONE` is only `==` to a `valueOf(1)` through them
    // (`R13Bigdec2Residuals` identity row). Before `<clinit>` has filled a
    // cache the static reads `null` and this falls through to a fresh object.
    if w9_switch(ctx, W9_JDK_IDENTITY) {
        let cached = match v {
            0 => bignum_static(ctx, BignumStatic::BiZero),
            1..=16 => bignum_static_element(ctx, BignumStatic::BiPosConst, v as usize),
            -16..=-1 => bignum_static_element(ctx, BignumStatic::BiNegConst, v.unsigned_abs() as usize),
            _ => None,
        };
        if let Some(o) = cached {
            return Ok(Some(Value::Object(Some(o))));
        }
    }
    // Fresh limb-backed object, no decimal round-trip.
    let result = bi_alloc_int(ctx, &bigint_from_i64(v));
    Ok(Some(Value::Object(Some(result?))))
}

/// A JDK `static final` constant of `BigInteger`/`BigDecimal` that the JDK's
/// own methods return by identity.
#[derive(Clone, Copy)]
enum BignumStatic {
    BiZero = 0,
    BiOne = 1,
    BiPosConst = 2,
    BiNegConst = 3,
    BdZeroThroughTen = 4,
    /// `BigDecimal.ZERO_SCALED_BY` (round 13 wave 12, lane bigdec3).
    BdZeroScaledBy = 5,
}

thread_local! {
    // `(vm, class, static slot)` per `BignumStatic`, memoised once resolved
    // (same per-VM scoping as `BD_LAYOUT_TLS`).
    static BIGNUM_STATIC_TLS: std::cell::Cell<[Option<(usize, ClassId, usize)>; 6]> =
        const { std::cell::Cell::new([None; 6]) };
}

/// The current value of `which`, or `None` when the class or field does not
/// resolve (synthetic JDK) or the field is still `null` (before `<clinit>`
/// assigned it).
fn bignum_static(ctx: &dyn NativeContext, which: BignumStatic) -> Option<ObjectRef> {
    let (class, field) = match which {
        BignumStatic::BiZero => ("java/math/BigInteger", "ZERO"),
        BignumStatic::BiOne => ("java/math/BigInteger", "ONE"),
        BignumStatic::BiPosConst => ("java/math/BigInteger", "posConst"),
        BignumStatic::BiNegConst => ("java/math/BigInteger", "negConst"),
        BignumStatic::BdZeroThroughTen => ("java/math/BigDecimal", "ZERO_THROUGH_TEN"),
        BignumStatic::BdZeroScaledBy => ("java/math/BigDecimal", "ZERO_SCALED_BY"),
    };
    let vm = ctx.vm_identity();
    let slot = which as usize;
    let memo = BIGNUM_STATIC_TLS.with(|c| c.get()[slot]);
    let (cid, idx) = match memo {
        Some((owner, cid, idx)) if owner == vm => (cid, idx),
        _ => {
            let cid = ctx.class_id_by_name(class)?;
            let idx = ctx.static_field_index_by_name(cid, field)?;
            BIGNUM_STATIC_TLS.with(|c| {
                let mut all = c.get();
                all[slot] = Some((vm, cid, idx));
                c.set(all);
            });
            (cid, idx)
        }
    };
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Element `index` of the constant array `which`, when it is there.
fn bignum_static_element(
    ctx: &dyn NativeContext,
    which: BignumStatic,
    index: usize,
) -> Option<ObjectRef> {
    let arr = bignum_static(ctx, which)?;
    if !ctx.object_is_array(arr) || index >= ctx.array_length(arr) {
        return None;
    }
    match ctx.get_array_element(arr, index) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(result?))))
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
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bi_negate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb path — sign flip only, no decimal round-trip.
    let a = bi_read_int(ctx, this);
    let result = bi_alloc_int(ctx, &a.neg_value());
    Ok(Some(Value::Object(Some(result?))))
}

/// `BigInteger.negate()` as the JDK writes it, `new BigInteger(this.mag,
/// -this.signum)`: a new object that SHARES the receiver's `mag` array (round
/// 13 wave 9, lane bigdec2, `CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG`).
///
/// `native_bi_negate` copied the magnitude out and into a fresh array -- two
/// O(words) passes and an array allocation per call, on the path Toom-Cook's
/// `subtract`/`negate` take in its inner steps. `mag` is never written after
/// construction (the class's immutability contract, which the JDK's own
/// `negate`/`abs` already rely on). Outside the real-JDK layout it defers to
/// the copying body.
fn native_bi_negate_shared(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let Some((sig_i, mag_i)) = bi_layout(ctx) else {
        return native_bi_negate(ctx, args);
    };
    let signum = match (ctx.get_field(this, sig_i), ctx.get_field(this, mag_i)) {
        (Value::Int(s), Value::Object(Some(_))) => s,
        _ => return native_bi_negate(ctx, args),
    };
    // GC-SAFETY: the allocation can move `this`; `mag` is read from the
    // forwarded receiver after it, and nothing allocates between that read
    // and the store.
    let h = ctx.pin_native_root(this);
    let obj = match bignum_alloc(ctx, BignumClass::Integer, 2) {
        Ok(o) => o,
        Err(e) => {
            ctx.unpin_native_roots(h);
            return Err(e);
        }
    };
    let this = ctx.read_native_pin(h, this);
    ctx.unpin_native_roots(h);
    let mag = ctx.get_field(this, mag_i);
    ctx.set_field(obj, sig_i, Value::Int(signum.wrapping_neg()));
    ctx.set_field(obj, mag_i, mag);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_bi_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Word-based limb path — sign clear only, no decimal round-trip (the
    // `negate` twin above already worked this way).
    let a = bi_read_int(ctx, this);
    let result = bi_alloc_int(ctx, &a.abs_value());
    Ok(Some(Value::Object(Some(result?))))
}

/// The runtime class name of an object, when the VM can tell us. `None` means
/// "could not resolve" and every caller here treats that as "do not refuse":
/// a type screen that fires on an unresolvable name would break working paths,
/// and this family's job is to stop *fabricating*, not to invent refusals.
fn bi_runtime_class_name(ctx: &dyn NativeContext, o: ObjectRef) -> Option<String> {
    ctx.class_name_of_id(ctx.class_id_of_object(o))
}

/// `true` when `other` is positively known NOT to be a `java.math.BigInteger`,
/// given a `this` that is one.
///
/// `BigInteger` is `final`, so an exact class match is the whole subtype test.
/// The fast arm is two `ClassId` reads and no allocation — `compareTo` is on
/// BouncyCastle's field-arithmetic and Lucene's `TestUtil.nextLong` hot paths,
/// so a per-call `String` would have paid for this screen out of throughput.
/// The name lookup runs only when the ids already differ, i.e. only on calls
/// that are about to refuse anyway (or on the one shape that is not a refusal:
/// two `BigInteger` classes from different loaders).
fn bi_is_definitely_not_biginteger(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> bool {
    if ctx.class_id_of_object(this) == ctx.class_id_of_object(other) {
        return false;
    }
    matches!(bi_runtime_class_name(ctx, other), Some(n) if n != "java/math/BigInteger")
}

/// `NullPointerException` for `BigInteger.compareTo(null)`, transcribed.
///
/// MEASURED on Temurin OpenJDK 25.0.3+9 (`scratchpad/g10/Bd2Probe.java`):
///
/// ```text
/// BigInteger.ONE.compareTo(null)
///   !! java.lang.NullPointerException: Cannot read field "signum" because "val" is null
/// ```
///
/// This is HotSpot's helpful-NPE naming `compareTo`'s own parameter, so it is
/// transcribed and not derived — deriving it would have produced the JDK 8
/// message (`null`) or a guess at the field.
fn bi_compare_to_null_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some("Cannot read field \"signum\" because \"val\" is null".to_string()),
    }
    .into()
}

fn native_bi_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // `compareTo(null)` used to answer **0** — "these two are equal" — from a
    // native registered for BOTH the typed and the erased descriptor. That is
    // fabricated success in the worst place for it: a `TreeMap` or a
    // `Collections.sort` over a list containing one null silently produced an
    // ordering instead of throwing. HotSpot NPEs; see `bi_compare_to_null_npe`.
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        Some(Value::Object(None)) | None => return Err(bi_compare_to_null_npe()),
        _ => return Ok(Some(Value::Int(0))),
    };
    // The erased `Comparable.compareTo(Object)` bridge is registered by the
    // same registrar, so a non-`BigInteger` argument reaches here where the
    // real bridge bytecode would have run `checkcast java/math/BigInteger`
    // first. Reading a `java.lang.String` through `BigInteger`'s field layout
    // answered *something* — a comparison result computed out of another
    // class's slots.
    //
    // MEASURED: `((Comparable) BigInteger.ONE).compareTo("1")` !!
    // `ClassCastException: class java.lang.String cannot be cast to class
    // java.math.BigInteger (java.lang.String and java.math.BigInteger are in
    // module java.base of loader 'bootstrap')`. The parenthetical is built by
    // the VM from the two classes' modules and loaders; that builder lives in
    // `vm/src/runtime/exceptions.rs` and is not reachable from this crate, so
    // the message here is the class-correct prefix ONLY. Wrong message text
    // (class (e)) in place of a fabricated answer (class (b)) — and a
    // NOMINATION to route it through the canonical builder.
    if bi_is_definitely_not_biginteger(ctx, this, other) {
        let name = bi_runtime_class_name(ctx, other)
            .unwrap_or_else(|| "java/lang/Object".to_string())
            .replace('/', ".");
        return Err(RuntimeError::ClassCastException {
            message: format!("class {name} cannot be cast to class java.math.BigInteger"),
        }
        .into());
    }
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
        // `equals(null)` is `false`, MEASURED. That arm was already right.
        _ => return Ok(Some(Value::Int(0))),
    };
    // `BigInteger.equals(Object)`'s first line is
    // `if (!(x instanceof BigInteger xInt)) return false;`
    // (`BigInteger.java:3806`). This body had no such line: it read ANY
    // argument through `BigInteger`'s `signum`+`mag` slots, so
    // `BigInteger.ONE.equals("1")` compared a `String`'s field slots against a
    // magnitude and answered whatever they happened to hold. MEASURED:
    // `ONE.equals("1")` = false, `ONE.equals(Integer.valueOf(1))` = false.
    if bi_is_definitely_not_biginteger(ctx, this, other) {
        return Ok(Some(Value::Int(0)));
    }
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
    // Only the `CRATONVM_BIGNUM_STRINGS_UNINTERNED=0` fallback reaches this
    // body (round 13 wave 13); that switch restores INTERNING, so it names the
    // inserting constructor, which stays inserting when `create_string` turns
    // hit-or-fresh.
    let java_str = ctx.intern_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// `native_bi_to_string` with a fresh, uninterned result (round 13 wave 8,
/// `CRATONVM_BIGNUM_STRINGS_UNINTERNED`). `create_string` puts the text in
/// the VM's string pool, which is a STRONG GC root that is never pruned
/// (`vm/src/memory/roots.rs`): every distinct `BigInteger.toString()` was
/// kept for the life of the VM, re-scanned by every collection, and `==` to
/// an equal literal (HotSpot returns a new `String`).
fn native_bi_to_string_uninterned(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = bi_read_int(ctx, this).to_decimal();
    let java_str = ctx.create_string_uninterned(&s);
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
        // Round 13 wave 9 (lane bigdec2): the chunked limb conversion (one
        // word division per nine digits, not per digit) and an uninterned
        // result -- HotSpot returns a new `String`, and the pool never shrinks.
        let a = bi_read_int(ctx, this).to_decimal();
        let result = ctx.create_string_uninterned(&a);
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
    let result = ctx.create_string_uninterned(&s);
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
    let base = bi_read_int(ctx, this);
    // JDK 25 `BigInteger.pow` (BigInteger.java:2594-2600) answers the trivial
    // shapes before it computes anything, and `ZERO.pow(Integer.MAX_VALUE)` /
    // `ONE.pow(Integer.MAX_VALUE)` are among them:
    //
    //     if (exponent == 0 || this.equals(ONE)) return ONE;
    //     if (signum == 0 || exponent == 1)      return this;
    //
    // MEASURED (`scratchpad/f7/Pow.java`, Microsoft OpenJDK 25.0.3+9):
    // `ZERO.pow(MAX) = 0 [0 ms]`, `ONE.pow(MAX) = 1 [0 ms]`.
    let one = BigInt::from_decimal("1");
    if exp == 0 || base == one {
        // `return ONE;` -- the JDK's constant (round 13 wave 9,
        // `CRATONVM_BIGNUM_JDK_IDENTITY`).
        if w9_switch(ctx, W9_JDK_IDENTITY) {
            if let Some(o) = bignum_static(ctx, BignumStatic::BiOne) {
                return Ok(Some(Value::Object(Some(o))));
            }
        }
        return Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &one)?))));
    }
    if base.is_zero() || exp == 1 {
        // `return this;` in the JDK (round 13 wave 9, `CRATONVM_BIGNUM_JDK_IDENTITY`).
        if w9_switch(ctx, W9_JDK_IDENTITY) {
            return Ok(Some(Value::Object(Some(this))));
        }
        return Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &base)?))));
    }
    // Refuse the argument-driven blow-up BEFORE allocating for it — see
    // `bi_pow_check_range`.
    bi_pow_check_range(&base, exp)?;
    // Square-and-multiply on binary limbs. The old implementation was an
    // O(exp) loop of decimal-string schoolbook multiplies with a full
    // BigInteger heap allocation per step — `new BigDecimal(double)` runs
    // 5^52 through here (real-JDK bytecode delegates to BigInteger.pow), so
    // that O(exp) loop was a measured ~114us per BigDecimal(double) ctor in
    // Lucene's TestUtil.nextLong hot path (ES codec/doc-values test hangs).
    let result = bigint_pow_limbs(&base, exp as u32);
    let obj = bi_alloc_int(ctx, &result);
    Ok(Some(Value::Object(Some(obj?))))
}

/// `base^exp` by square-and-multiply on limbs, for a caller that has already
/// answered the trivial shapes and run [`bi_pow_check_range`].
///
/// With `CRATONVM_BIGINT_FAST_MUL` on (the default) it also takes the JDK's
/// own shortcut (`BigInteger.pow`: `getLowestSetBit`, `shiftRight`, and one
/// `shiftLeft(powersOfTwo * exponent)` at the end): the trailing zero bits of
/// the base are exponentiated by a shift instead of being squared along. For
/// `TEN.pow(n)` -- `bigTenToThe`, every large `BigDecimal` rescale -- that
/// squares `5^k` instead of `10^k`, about half the limb products. The shift
/// cannot overflow: `bi_pow_check_range` refused `powersOfTwo * exponent >
/// Integer.MAX_VALUE` already.
fn bigint_pow_limbs(base: &crate::bigint::BigInt, exp: u32) -> crate::bigint::BigInt {
    use crate::bigint::BigInt;
    let tz = if !base.is_zero() && crate::bigint::fast_mul_enabled() {
        base.lowest_set_bit().max(0) as u32
    } else {
        0
    };
    // An odd base (tz == 0) keeps its sign through the signed multiplies.
    let (odd, twos) = if tz > 0 {
        (base.abs_value().shr(tz), tz)
    } else {
        (base.clone(), 0)
    };
    let mut result = BigInt::from_decimal("1");
    let mut sq = odd;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = result.mul(&sq);
        }
        e >>= 1;
        if e > 0 {
            sq = sq.mul(&sq);
        }
    }
    if twos > 0 {
        result = result.shl(twos.saturating_mul(exp));
        // The factored path squared |base|; the sign is the JDK's
        // `signum < 0 && (exponent & 1) == 1`.
        if base.is_neg() && exp & 1 == 1 {
            result = result.neg_value();
        }
    }
    result
}

/// `BigInteger.pow(exponent)` for a caller holding a limb value (the
/// `BigDecimal` natives' `this.inflated().pow(n)`): the JDK's trivial answers,
/// its range refusal, then [`bigint_pow_limbs`]. `exp < 0` is refused like
/// `BigInteger.pow` refuses it.
fn bigint_pow_checked(
    base: &crate::bigint::BigInt,
    exp: i32,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    use crate::bigint::BigInt;
    if exp < 0 {
        return Err(RuntimeError::ArithmeticException {
            message: "Negative exponent".to_string(),
        }
        .into());
    }
    let one = BigInt::from_decimal("1");
    if exp == 0 || *base == one {
        return Ok(one);
    }
    if base.is_zero() || exp == 1 {
        return Ok(base.clone());
    }
    bi_pow_check_range(base, exp)?;
    Ok(bigint_pow_limbs(base, exp as u32))
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
    // Same per-VM thread-local memo as `bi_layout` — see the comment there for
    // why it is scoped by `vm_identity()` and why only a successful resolve is
    // stored. Four name-keyed resolves per call, on every BigDecimal native.
    if let Some(cached) = bd_layout_cached(ctx.vm_identity()) {
        return Some(cached);
    }
    let iv = ctx.resolve_field_index("java/math/BigDecimal", "intVal")?;
    let sc = ctx.resolve_field_index("java/math/BigDecimal", "scale")?;
    let pr = ctx.resolve_field_index("java/math/BigDecimal", "precision")?;
    let ic = ctx.resolve_field_index("java/math/BigDecimal", "intCompact")?;
    bd_layout_store(ctx.vm_identity(), (iv, sc, pr, ic));
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

fn bd_alloc(
    ctx: &mut dyn NativeContext,
    value: &str,
    scale: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = bignum_alloc(ctx, BignumClass::Decimal, 3)?;
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
            // A refused allocation releases the pin before it propagates
            // (round 13 wave 9; see `bd_unpin_on_err`).
            let bi = match bi_alloc(ctx, &unscaled_str) {
                Ok(bi) => bi,
                Err(e) => {
                    ctx.unpin_native_roots(h);
                    return Err(e);
                }
            };
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
                match bi_alloc(ctx, &unscaled_str) {
                    Ok(bi) => Some(bi),
                    Err(e) => {
                        ctx.unpin_native_roots(h);
                        return Err(e);
                    }
                }
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
        // Uninterned (round 13 wave 9), like `bi_alloc`'s synthetic arm.
        let s = ctx.create_string_uninterned(value);
        let obj = ctx.read_native_pin(h, obj);
        ctx.set_field(obj, BD_FIELD_VALUE, Value::Object(Some(s)));
        ctx.set_field(obj, BD_FIELD_SCALE, Value::Int(scale));
        ctx.set_field(obj, BD_FIELD_PRECISION, Value::Int(precision));
    }
    let obj = ctx.read_native_pin(h, obj);
    ctx.unpin_native_roots(h);
    Ok(obj)
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
    // Round 13 wave 13 (lane bigdec4): was the interning `native_bd_to_string`
    // unconditionally. The cached body renders a fresh `String` and, with no
    // `stringCache` field in the synthetic layout, simply does not cache.
    let bd_to_string_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_TOSTRING_CACHE") {
            native_bd_to_string_cached
        } else {
            native_bd_to_string
        };
    registry.register(bd, "toString", "()Ljava/lang/String;", bd_to_string_body);
    // Round 13 wave 12 (lane bigdec3): this slot held `native_bd_to_string`,
    // the CANONICAL layout, so `new BigDecimal("1E+3").toPlainString()` was
    // "1E+3" (JDK "1000") under `--features synthetic-jdk`.
    registry.register(
        bd,
        "toPlainString",
        "()Ljava/lang/String;",
        native_bd_to_plain_string_uninterned,
    );
    registry.register(bd, "intValue", "()I", native_bd_int_value);
    registry.register(bd, "longValue", "()J", native_bd_long_value);
    let (bd_double_value_body, bd_float_value_body) = bd_double_float_value_bodies();
    registry.register(bd, "doubleValue", "()D", bd_double_value_body);
    registry.register(bd, "floatValue", "()F", bd_float_value_body);
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
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "0", 0)?))))
    });
    registry.register(bd, "ONE", "()Ljava/math/BigDecimal;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "1", 0)?))))
    });
    registry.register(bd, "TEN", "()Ljava/math/BigDecimal;", |ctx, _args| {
        Ok(Some(Value::Object(Some(bd_alloc(ctx, "10", 0)?))))
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

/// `BigDecimal.toPlainString()`'s own two refusals, ported from JDK 25
/// `BigDecimal.java:3425-3455` (the negative-scale road) and
/// `getValueString`/`BigDecimal.java:3458-3482` (the positive-scale road).
///
/// # Why this is a separate predicate and not a line inside `apply_scale`
///
/// [`apply_scale`] returns a `String`, so it cannot refuse; lane F20 removed
/// the VM abort from it (`-i32::MIN` → `scale.unsigned_abs()`) and left the
/// refusal nominated, because making it fallible is a ripple through every
/// caller. The ripple is [`bd_read`]'s, one function down, and this is it.
///
/// # The two messages are NOT interchangeable, and neither is the sign
///
/// `toPlainString` calls the **static** `checkScaleNonZero(-(long) scale)`,
/// which *casts*: `-(long) Integer.MIN_VALUE` is `2147483648`, `(int)` of it is
/// `Integer.MIN_VALUE`, which is negative, so the message is `"Overflow"`.
/// `setScale`/`toBigInteger` call the **instance** `checkScale`, which *clamps*
/// to `Integer.MAX_VALUE` first and therefore says `"Underflow"` for the same
/// scale — see [`bd_to_big_integer_check`]. Both were measured on the same
/// receiver; do not unify them.
///
/// # MEASURED
///
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build) on this
/// host, `scratchpad/f31/{Bd,Bd2,Bd4,Bd5}.java`. `bd(u,s)` is
/// `new BigDecimal(BigInteger.valueOf(u), s)`:
///
/// ```text
/// bd(1,MIN).toPlainString()          !! ArithmeticException: Overflow                     [0 ms]
/// bd(-1,MIN).toPlainString()         !! ArithmeticException: Overflow                     [0 ms]
/// bd(0,MIN).toPlainString()           = 0                                                 [0 ms]
/// bd(0,-5).toPlainString()            = 0                                                 [0 ms]
/// bd(0,5).toPlainString()             = 0.00000                                           [0 ms]
/// bd(0,MAX).toPlainString()          !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// bd(1,-2147483647).toPlainString()  !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// bd(1,-2147483646).toPlainString()  !! OutOfMemoryError: Requested array size exceeds VM limit [0 ms]
/// bd(12,-2147483646).toPlainString() !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// bd(-1,-2147483646).toPlainString() !! OutOfMemoryError: too large to fit in a String  [120 ms]
/// bd(-1,-2147483645).toPlainString() !! OutOfMemoryError: Requested array size exceeds VM limit [0 ms]
/// bd(1,2147483646).toPlainString()   !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// bd(1,2147483645).toPlainString()   !! OutOfMemoryError: Requested array size exceeds VM limit [0 ms]
/// bd(-1,2147483645).toPlainString()  !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// bd(123,3).toPlainString()           = 0.123      bd(-123,4).toPlainString() = -0.0123
/// ```
///
/// Three things the rows pin that a guess would get wrong:
///
/// 1. The `signum() == 0` short-circuit is on the **negative-scale road only**.
///    `bd(0,MAX)` is an `OutOfMemoryError`, not `"0"` — which is exactly why
///    [`apply_scale`]'s short-circuit is `unscaled == "0" && scale < 0`.
/// 2. On the negative road `len` is `str.length() + trailingZeros` where `str`
///    is the **signed** rendering: `bd(-1,-2147483646)` refuses and
///    `bd(1,-2147483646)` does not, one character apart.
/// 3. On the positive road `len` is `(signum < 0 ? 3 : 2) + scale` — the
///    unscaled digits are *not* in it: `bd(-1,2147483645)` refuses and
///    `bd(1,2147483645)` does not.
///
/// The `Requested array size exceeds VM limit` rows are NOT ported: that is
/// HotSpot's array-length ceiling firing inside `StringBuilder`, not a
/// `BigDecimal` screen, and porting it here would be inventing a cap in the one
/// direction F20 warned about. See this lane's record, §Residuals.
fn bd_plain_string_check(unscaled: &str, scale: i32) -> Result<(), MethodCallFailed> {
    if scale == 0 {
        return Ok(());
    }
    let neg = unscaled.starts_with('-');
    // `intVal.abs().toString().length()` — the positive road's `intString`.
    // `saturating_sub` so an empty reading cannot underflow the subtraction:
    // this whole family exists because one unchecked arithmetic op took the VM
    // down, and `usize` underflow is the same defect one type over.
    let digits = unscaled.len().saturating_sub(usize::from(neg));
    if scale < 0 {
        // `if (signum() == 0) return "0";` runs BEFORE the scale is validated,
        // so a zero value takes any scale (row 3 above).
        if unscaled == "0" {
            return Ok(());
        }
        // `int trailingZeros = checkScaleNonZero((-(long)scale));`
        if scale == i32::MIN {
            return Err(RuntimeError::ArithmeticException {
                message: "Overflow".to_string(),
            }
            .into());
        }
        // `int len = str.length() + trailingZeros; if (len < 0) throw …`
        // `-i64::from(scale)`, NOT `i64::from(-scale)`: the i32::MIN arm above
        // already returned, but negating an i32 that a caller chose is the
        // exact shape this family exists to remove — do not spell it that way.
        if unscaled.len() as i64 - i64::from(scale) > i64::from(i32::MAX) {
            return Err(bd_string_too_large());
        }
        return Ok(());
    }
    // `getValueString`: only the `insertionPoint < 0` arm has a screen.
    if (digits as i64) < i64::from(scale)
        && i64::from(if neg { 3 } else { 2 }) + i64::from(scale) > i64::from(i32::MAX)
    {
        return Err(bd_string_too_large());
    }
    Ok(())
}

/// `OutOfMemoryError("too large to fit in a String")` — JDK 25
/// `BigDecimal.toPlainString`/`getValueString`. Both sites use the identical
/// text, so it lives in one place.
fn bd_string_too_large() -> MethodCallFailed {
    RuntimeError::OutOfMemoryError {
        message: "too large to fit in a String".to_string(),
    }
    .into()
}

/// `toPlainString()`'s rendering, WITH `toPlainString()`'s refusals.
///
/// FALLIBLE since 2026-08-13 (F20-1 N2). It has exactly one caller,
/// `native_bd_to_plain_string`, and that is not an accident — see
/// [`bd_read_unchecked`], which every other caller takes.
fn bd_read(ctx: &dyn NativeContext, this: ObjectRef) -> Result<String, MethodCallFailed> {
    if let Some((unscaled, scale)) = bd_read_parts(ctx, this) {
        bd_plain_string_check(&unscaled, scale)?;
        return Ok(apply_scale(&unscaled, scale));
    }
    Ok(bd_read_stub(ctx, this))
}

/// The same plain rendering with **no refusal** — the internal arithmetic form.
///
/// # This split is not tidiness; merging the two is 12 fresh divergences
///
/// `bd_read` used to be one infallible function with 13 call sites, and the
/// obvious way to satisfy F20-1 N2 is to make it fallible and put a `?` on all
/// 13. That is wrong, and it is wrong in the direction this whole lane is about:
/// **`toPlainString()` is the only one of those methods HotSpot ever refuses.**
/// None of the others renders the value at all — `equals` compares `scale` then
/// the unscaled `BigInteger`, `hashCode` is `31*intVal.hashCode() + scale`,
/// `doubleValue` has its own fast paths, `compareTo` compares magnitudes.
///
/// MEASURED on `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build),
/// `scratchpad/f31/{Bd6,Bd7}.java`, on the very receivers
/// [`bd_plain_string_check`] refuses:
///
/// ```text
/// bd(1,MIN).equals(bd(1,MIN))     = true            [0 ms]   bd(1,MIN).equals(bd(1,MAX)) = false
/// bd(1,MIN).hashCode()            = -2147483617     [0 ms]   bd(1,MAX).hashCode() = -2147483618
/// bd(1,MIN).doubleValue()         = Infinity        [0 ms]   bd(1,MAX).doubleValue()  = 0.0
/// bd(-1,MIN).doubleValue()        = -Infinity       [0 ms]   bd(-1,MAX).doubleValue() = -0.0
/// bd(1,MIN).floatValue()          = Infinity        [0 ms]   bd(1,MAX).floatValue()   = 0.0
/// bd(1,MIN).stripTrailingZeros()  = 1E+2147483648   [5 ms]   bd(1,MAX).stripTrailingZeros() = 1E-2147483647
/// bd(1,MIN).compareTo(bd(1,MAX))  = 1               [0 ms]   bd(1,MAX).compareTo(bd(1,MAX)) = 0
/// bd(1,MIN).divide(bd(1,MIN))     = 1               [0 ms]   bd(1,MAX).divide(bd(1,MAX))    = 1
/// bd(1,MIN).divide(bd(2,0),2,HALF_UP) = 0.01        [0 ms]
///
/// bd(1,MIN).toPlainString()      !! ArithmeticException: Overflow                     [0 ms]
/// bd(1,MAX).toPlainString()      !! OutOfMemoryError: too large to fit in a String    [0 ms]
/// ```
///
/// Twelve rows that answer in 0 ms, one method that refuses. A `?` on all 13
/// call sites would have refused all of it.
///
/// # RESIDUAL, recorded rather than capped
///
/// These twelve callers still build the `scale`-sized string that HotSpot never
/// builds, so the denial of service is real and is NOT fixed here. It cannot be
/// fixed by refusing — HotSpot answers — only by not rendering: `doubleValue`/
/// `floatValue`/`compareTo`/`divide` want an `f64` that
/// `(unscaled, scale)` determines directly, `equals` wants `scale` then the
/// unscaled `BigInt`, and `hashCode` wants the JDK's own two-term hash. That is
/// a precision-axis change, not an allocation-axis one, and it is nominated
/// rather than guessed at:
/// `docs/internal/jdk-only/F31-1-three-roads-out-of-one-scale-and-the-zero-operand-that-is-exempt-20260813.md`
/// §10. What this function does NOT do is turn that residual into a divergence.
fn bd_read_unchecked(ctx: &dyn NativeContext, this: ObjectRef) -> String {
    if let Some((unscaled, scale)) = bd_read_parts(ctx, this) {
        return apply_scale(&unscaled, scale);
    }
    bd_read_stub(ctx, this)
}

/// Synthetic-stub fallback shared by the two readings above — the value is
/// already a decimal string there and carries no separate scale to validate,
/// so this arm is infallible in both.
fn bd_read_stub(ctx: &dyn NativeContext, this: ObjectRef) -> String {
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
    bd_write_into(ctx, this, &s, scale)?;
    Ok(None)
}

/// Populate an existing `BigDecimal` instance from a decimal string + scale.
/// Picks the layout (real-JDK intVal/scale/precision/intCompact vs. legacy
/// synthetic value/scale/precision) automatically.
fn bd_write_into(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    value: &str,
    scale: i32,
) -> Result<(), MethodCallFailed> {
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
        let bi = bd_unpin_on_err(ctx, h, bi)?;
        ctx.set_field(this, iv_i, Value::Object(Some(bi)));
        ctx.set_field(this, ic_i, Value::Long(ic));
        ctx.set_field(this, sc_i, Value::Int(scale));
        ctx.set_field(this, pr_i, Value::Int(precision));
    } else {
        // Uninterned (round 13 wave 9), like `bi_alloc`'s synthetic arm.
        let val_str = ctx.create_string_uninterned(value);
        let this = ctx.read_native_pin(h, this);
        ctx.set_field(this, BD_FIELD_VALUE, Value::Object(Some(val_str)));
        ctx.set_field(this, BD_FIELD_SCALE, Value::Int(scale));
        ctx.set_field(this, BD_FIELD_PRECISION, Value::Int(precision));
    }
    ctx.unpin_native_roots(h);
    Ok(())
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
) -> Result<(), MethodCallFailed> {
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
            return Ok(());
        }
        // Inflated: pin `this` across the BigInteger allocation (GC-SAFETY —
        // see `bd_write_into`).
        let h = ctx.pin_native_root(this);
        let bi = bi_alloc_int(ctx, unscaled);
        let this = ctx.read_native_pin(h, this);
        let bi = bd_unpin_on_err(ctx, h, bi)?;
        ctx.set_field(this, iv_i, Value::Object(Some(bi)));
        ctx.set_field(this, ic_i, Value::Long(BD_INFLATED));
        ctx.set_field(this, sc_i, Value::Int(scale));
        ctx.set_field(this, pr_i, Value::Int(precision));
        ctx.unpin_native_roots(h);
        return Ok(());
    }
    let value = apply_scale(&unscaled.to_decimal(), scale);
    bd_write_into(ctx, this, &value, scale)?;
    Ok(())
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
        // Round 13 wave 12 (lane bigdec3): the JDK stores the constant
        // `BigInteger.ZERO` itself (`new BigDecimal(0.0).unscaledValue() ==
        // BigInteger.ZERO` on HotSpot), not a fresh zero
        // (`CRATONVM_BIGNUM_JDK_IDENTITY`; `null` before `<clinit>` allocates).
        let shared_zero = if w9_switch(ctx, W9_JDK_IDENTITY) {
            bignum_static(ctx, BignumStatic::BiZero)
        } else {
            None
        };
        let zero = match shared_zero {
            Some(z) => Ok(z),
            None => bi_alloc_int(ctx, &BigInt::zero()),
        };
        let this = ctx.read_native_pin(h, this);
        if let Some((iv_i, sc_i, pr_i, ic_i)) = bd_layout(ctx) {
            let zero = bd_unpin_on_err(ctx, h, zero)?;
            ctx.set_field(this, iv_i, Value::Object(Some(zero)));
            ctx.set_field(this, ic_i, Value::Long(0));
            ctx.set_field(this, sc_i, Value::Int(0));
            ctx.set_field(this, pr_i, Value::Int(1));
        } else {
            let s = ctx.create_string("0");
            // gc-common w20-c: that allocation can move `this` too.
            let this: ObjectRef = ctx.read_native_pin(h, this);
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
        (mag.mul(&bigint_pow5_memo((-exp) as u32)), -exp)
    };
    let unscaled = if neg { unscaled.neg_value() } else { unscaled };
    bd_write_into_bigint(ctx, this, &unscaled, scale, 0)?;
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
            // MEASURED on Temurin OpenJDK 25.0.3+9 (`scratchpad/g10/BdProbe.java`):
            //
            //     new BigDecimal((BigInteger) null)
            //       !! NullPointerException: Cannot invoke "Object.getClass()" because "val" is null
            //
            // The message is HotSpot's helpful-NPE naming the real ctor's own
            // parameter and the first thing it dereferences (`compactValFor`'s
            // `val.getClass()`), so it is transcribed, not derived. This arm
            // threw a message-less NPE, which is the right exception with the
            // wrong text.
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"Object.getClass()\" because \"val\" is null".to_string(),
                ),
            }
            .into());
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
        // Round 13 wave 13 (lane bigdec4; `CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS`):
        // JDK 25 is `intVal = toStrictBigInteger(val); intCompact =
        // compactValFor(intVal)` -- it keeps the argument for a COMPACT value
        // too (so `unscaledValue()`/`toBigInteger()` answer that very object),
        // and a subclass instance is first copied into a plain `BigInteger`
        // (`toStrictBigInteger`: a subclass's overrides must not run inside
        // `BigDecimal`). This body dropped the compact argument and kept an
        // inflated subclass instance as is.
        let jdk_ctor = w9_switch(ctx, W13_MC_RESULT_CONSTANTS);
        let exact_class =
            bi_runtime_class_name(ctx, bi_obj).as_deref() == Some("java/math/BigInteger");
        match compact {
            Some(ic) => {
                let keep = if jdk_ctor && exact_class {
                    Some(bi_obj)
                } else {
                    None
                };
                ctx.set_field(this, iv_i, Value::Object(keep));
                ctx.set_field(this, ic_i, Value::Long(ic));
            }
            None if jdk_ctor && !exact_class => {
                // GC-SAFETY: the copy's allocation can move `this`; pin it across.
                let h = ctx.pin_native_root(this);
                let copy = bi_alloc_int(ctx, &v);
                let this = ctx.read_native_pin(h, this);
                ctx.unpin_native_roots(h);
                ctx.set_field(this, iv_i, Value::Object(Some(copy?)));
                ctx.set_field(this, ic_i, Value::Long(BD_INFLATED));
                ctx.set_field(this, sc_i, Value::Int(0));
                ctx.set_field(this, pr_i, Value::Int(0));
                return Ok(None);
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
    bd_write_into_bigint(ctx, this, &v, 0, 0)?;
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
    bd_write_into(ctx, this, &s, 0)?;
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
    bd_write_into(ctx, this, &s, 0)?;
    Ok(None)
}

fn native_bd_value_of_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(l)) => *l,
        _ => 0,
    };
    let result = bd_alloc(ctx, &v.to_string(), 0);
    Ok(Some(Value::Object(Some(result?))))
}

/// `BigDecimal.valueOf(long)` as ONE compact object (round 13 wave 9, lane
/// bigdec2, `CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT`): `intCompact = v`,
/// `intVal = null`, lazy precision -- the JDK's `new BigDecimal(null, val, 0,
/// 0)`. `native_bd_value_of_long` went through `bd_alloc`: a decimal
/// rendering, a re-parse, and a backing `BigInteger` plus its `mag` array,
/// three allocations for a value that fits the compact slot.
fn native_bd_value_of_long_compact(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(l)) => *l,
        _ => 0,
    };
    // `ZERO_THROUGH_TEN[(int) val]` for 0..=10, as the JDK answers
    // (`CRATONVM_BIGNUM_JDK_IDENTITY`; `null` before `<clinit>` falls through).
    if (0..=10).contains(&v) && w9_switch(ctx, W9_JDK_IDENTITY) {
        if let Some(o) = bignum_static_element(ctx, BignumStatic::BdZeroThroughTen, v as usize) {
            return Ok(Some(Value::Object(Some(o))));
        }
    }
    let result = bd_alloc_bigint(ctx, &bigint_from_i64(v), 0);
    Ok(Some(Value::Object(Some(result?))))
}

/// `BigDecimal.valueOf(double)` --- specified as
/// `new BigDecimal(Double.toString(val))`, so both halves of that sentence
/// have to hold.
///
/// This used to render the double with Rust's `format!("{}", d)` and then take
/// the scale from the position of the `.`. Rust's `Display` for `f64` is not
/// `Double.toString`: it never uses E-notation and it prints `2.0` as `2`. So
/// `valueOf(1e100)` produced a scale-0 integer with 101 digits instead of
/// unscaled 10 at scale -99, and `valueOf(2.0)` lost the trailing zero (scale 0
/// instead of 1). H2 renders a DOUBLE into JSON through
/// `ValueDouble.getBigDecimal()` -> `BigDecimal.valueOf(double)` ->
/// `BigDecimal.toString()`, which is why `CAST(1e100 AS JSON)` came back as 101
/// literal digits (`datatypes/json.sql:46`, `:49`).
///
/// `format_double` is the shared `Double.toString` formatter, and the mantissa
/// digits + exponent are turned straight into the exact `(unscaled, scale)`
/// pair rather than going back through a decimal string --- `bd_alloc`'s
/// string path has no E-notation handling and would strip significant trailing
/// zeros for a negative scale.
fn native_bd_value_of_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let d = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // NON-FINITE FIRST. `BigDecimal.valueOf(double)` is specified as
    // `new BigDecimal(Double.toString(val))`, and that parse rejects NaN and
    // both infinities. MEASURED on both VMs:
    //
    //   BigDecimal.valueOf(NaN)    HotSpot  NumberFormatException: Infinite or NaN
    //   BigDecimal.valueOf(+Inf)   HotSpot  NumberFormatException: Infinite or NaN
    //
    // This native REPLACES `valueOf` outright, so no `BigDecimal(String)`
    // parse happens and there is nothing left to throw: `format_double(NaN)`
    // is `"NaN"`, which flows through `bd_parts_of_java_double_string` as a
    // mantissa with no `.` and reaches `BigInt::from_decimal`, and all three
    // rows silently answered `0`. Returning a wrong NUMBER where the oracle
    // refuses is worse than the E-notation defect this function was written
    // to fix.
    //
    // `bd_parts_of_java_double_string`'s doc used to assert that non-finite
    // doubles "cannot reach here" because `valueOf` throws inside the parse.
    // That was true of the bytecode path and stopped being true the moment
    // this native took the slot; the assertion is corrected there.
    //
    // The message is transcribed, not composed — `java.math.BigDecimal`'s own
    // `Infinite or NaN`, with no value interpolated.
    if !d.is_finite() {
        return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: "Infinite or NaN".to_string(),
        }
        .into());
    }
    let (unscaled, scale) = bd_parts_of_java_double_string(&crate::lang_string::format_double(d));
    let result = bd_alloc_bigint(ctx, &crate::bigint::BigInt::from_decimal(&unscaled), scale);
    Ok(Some(Value::Object(Some(result?))))
}

/// Split a `Double.toString`-shaped string into the `(unscaled digits, scale)`
/// pair `new BigDecimal(String)` would produce.
///
/// `Double.toString` output is always `[-]<digit>.<digits>[E[-]<exp>]`, so the
/// scale is "digits after the point, less the exponent" --- e.g. `1.0E100` ->
/// (`10`, `1 - 100` = `-99`), `2.0` -> (`20`, `1`), `1.0E-7` -> (`10`, `8`).
/// Non-finite doubles do not reach here, because [`native_bd_value_of_double`]
/// refuses them BEFORE calling this — not, as this note previously claimed,
/// because "`valueOf` throws inside the `Double.toString`-fed
/// `BigDecimal(String)` parse". That was true of the bytecode path and stopped
/// being true the moment a native took the `valueOf` slot: there is no parse
/// left to throw. `"NaN"` arrives here as a mantissa with no `.` and leaves as
/// unscaled `NaN` at scale 0, which `BigInt::from_decimal` renders as `0` —
/// measured, three silently wrong rows.
///
/// The distinction matters beyond this function: a guard that lives in a body
/// you have replaced is not a guard you still have.
fn bd_parts_of_java_double_string(s: &str) -> (String, i32) {
    let (mantissa, exp) = match s.find(['E', 'e']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let frac_digits = match mantissa.find('.') {
        Some(p) => (mantissa.len() - p - 1) as i32,
        None => 0,
    };
    (mantissa.replace('.', ""), frac_digits - exp)
}

/// Parse a synthetic-layout `BigDecimal` value string EXACTLY, into the
/// `(unscaled, scale)` pair `java.math.BigDecimal(String)` would build from it.
///
/// Grammar, as the JDK constructor accepts it: an optional sign, digits with an
/// optional single `.`, and an optional exponent `[eE][+-]?digits`. The result
/// follows the JDK rule -- `unscaled` is every digit with the point removed and
/// `scale` is the number of fraction digits MINUS the exponent -- so `"1.5E+10"`
/// is `(15, -9)`, exactly as `new BigDecimal("1.5E+10").unscaledValue()` and
/// `.scale()` report it.
///
/// # Why this exists instead of `bd_unscaled_bigint`'s synthetic arm
///
/// That arm reads the string, does `s.replace('.', "")`, and hands the rest to
/// `BigInt::from_decimal` -- which SKIPS every non-digit rather than refusing
/// it. So `"1.5E+10"` became `"15E+10"` and then the integer 1510: not an
/// error, a wrong number. It also takes the scale from `BD_FIELD_SCALE`, a
/// separate field that nothing forces to agree with the string. Here the string
/// is the single source, the exponent is honoured, and `from_decimal` is only
/// ever given a string of ASCII digits.
///
/// `None` for anything outside the grammar, and for a scale that does not fit
/// the `i32` a `BigDecimal` scale is -- the caller falls back rather than
/// guessing.
fn bd_parse_decimal_str(s: &str) -> Option<(crate::bigint::BigInt, i32)> {
    let s = s.trim();
    let (neg, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(i) => {
            let exp = &rest[i + 1..];
            let digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (&rest[..i], exp.parse::<i64>().ok()?)
        }
        None => (rest, 0i64),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((a, b)) => (a, b),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = i64::try_from(frac_part.len()).ok()?.checked_sub(exponent)?;
    let scale = i32::try_from(scale).ok()?;
    let mut digits = String::with_capacity(1 + int_part.len() + frac_part.len());
    if neg {
        digits.push('-');
    }
    digits.push_str(int_part);
    digits.push_str(frac_part);
    Some((crate::bigint::BigInt::from_decimal(&digits), scale))
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
    // The synthetic layout's convention, which every producer here follows:
    // the string's DIGITS (ignoring any '.') are the unscaled value, and the
    // scale lives in `BD_FIELD_SCALE`. A rendered positive scale satisfies it
    // (a '.' adds no digit); a negative scale is stored as bare unscaled digits
    // and never rendered with trailing zeros; and a scale no string could hold
    // -- `new BigDecimal(ONE, Integer.MIN_VALUE)` -- is stored as "1" with the
    // scale beside it. So the stored scale is authoritative, and this reads it.
    //
    // The ONE exception is scientific notation: `BigInt::from_decimal` SKIPS
    // every non-digit, so "1.5E+10" became "15E+10" and then the integer 1510.
    // Only there does the string carry scale information the stored field may
    // not, and only there is it parsed exactly (`bd_parse_decimal_str`).
    //
    // An earlier revision of this arm parsed EVERY string exactly and ignored
    // the stored scale, on a theory that a negative scale rendered as "150"
    // would read back ten times too large. No producer renders a negative scale
    // -- the only one that did was that same revision's `divide` -- and the
    // change broke `bigdecimal_extreme_scale_refusals_f31`: "1" with scale
    // `Integer.MIN_VALUE` read back as 1, where HotSpot's `intValue()` is 0.
    if s.contains(['e', 'E']) {
        if let Some(pair) = bd_parse_decimal_str(&s) {
            return pair;
        }
    }
    (BigInt::from_decimal(&s.replace('.', "")), scale)
}

/// `r`, releasing the native pin `h` first when it is an error, so the pin
/// stack stays balanced on the error road too (round 13 wave 9, lane
/// bigdec2). A `?` between a `pin_native_root` and its `unpin_native_roots`
/// returned with the pin held; the native-call funnel truncates the pin list
/// when the native returns, so this was not a leak across calls, but a
/// helper's caller that continued after the error kept the stale pin.
fn bd_unpin_on_err<T>(
    ctx: &mut dyn NativeContext,
    h: usize,
    r: Result<T, MethodCallFailed>,
) -> Result<T, MethodCallFailed> {
    if r.is_err() {
        ctx.unpin_native_roots(h);
    }
    r
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
) -> Result<ObjectRef, MethodCallFailed> {
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
        let obj = bignum_alloc(ctx, BignumClass::Decimal, 3)?;
        if let Some(ic) = compact {
            ctx.set_field(obj, iv_i, Value::Object(None));
            ctx.set_field(obj, ic_i, Value::Long(ic));
            ctx.set_field(obj, sc_i, Value::Int(scale));
            ctx.set_field(obj, pr_i, Value::Int(0));
            return Ok(obj);
        }
        // Inflated: allocate the backing BigInteger. Pin `obj` across that
        // allocation (GC-SAFETY — see `bd_alloc`).
        let h = ctx.pin_native_root(obj);
        let bi = bi_alloc_int(ctx, unscaled);
        let obj = ctx.read_native_pin(h, obj);
        let bi = bd_unpin_on_err(ctx, h, bi)?;
        ctx.set_field(obj, iv_i, Value::Object(Some(bi)));
        ctx.set_field(obj, ic_i, Value::Long(BD_INFLATED));
        ctx.set_field(obj, sc_i, Value::Int(scale));
        ctx.set_field(obj, pr_i, Value::Int(0));
        ctx.unpin_native_roots(h);
        return Ok(obj);
    }
    // Synthetic-stub layout: fall back to the decimal-string path.
    let value = apply_scale(&unscaled.to_decimal(), scale);
    Ok(bd_alloc(ctx, &value, scale)?)
}

/// The JDK's cached constant for an `add`/`subtract`/`multiply`/`negate`
/// result, when JDK 25 answers one (round 13 wave 12, lane bigdec3,
/// `CRATONVM_BIGDECIMAL_RESULT_CONSTANTS`).
///
/// Every road of those four methods that can produce a small result ends in
/// `valueOf(long, int)` / `valueOf(long, int, int)` / `valueOf(BigInteger,
/// int, int)` (`BigDecimal.java` 1312-1367, 2569, 5239-5304, 5792-5805); the
/// roads that build `new BigDecimal(..)` directly are the same-sign inflated
/// sums and the inflated products, whose magnitude is at least `2^63`
/// (`setScale`, `BigDecimal.java:2877`, is the same: `valueOf`, `zeroValueOf`,
/// or `divideAndRound` -> `MutableBigInteger.toBigDecimal` -> `valueOf`). Those
/// `valueOf`s answer `ZERO_THROUGH_TEN[v]` for scale 0 and `0 <= v <= 10`, and
/// `ZERO_SCALED_BY[scale]` for a zero at `0 <= scale < 16`. So on HotSpot
/// `ONE.add(ONE) == valueOf(2)` and `x.subtract(x) == ZERO` (scale 0) hold;
/// the natives allocated a fresh object. `None` otherwise, and before
/// `<clinit>` has filled the arrays.
fn bd_result_constant(
    ctx: &dyn NativeContext,
    v: &crate::bigint::BigInt,
    scale: i32,
) -> Option<ObjectRef> {
    if !w9_switch(ctx, W12_RESULT_CONSTANTS) {
        return None;
    }
    bd_constant_for(ctx, v, scale)
}

/// The constant JDK 25's `valueOf(long, int[, int])` / `valueOf(BigInteger,
/// int, int)` / `zeroValueOf` answer for `(v, scale)`, switch-free (the
/// callers hold the switch that applies).
fn bd_constant_for(
    ctx: &dyn NativeContext,
    v: &crate::bigint::BigInt,
    scale: i32,
) -> Option<ObjectRef> {
    if v.is_zero() {
        // `ZERO_SCALED_BY[0]` is `ZERO_THROUGH_TEN[0]`; the array's own length
        // is the JDK's bound.
        return usize::try_from(scale)
            .ok()
            .and_then(|i| bignum_static_element(ctx, BignumStatic::BdZeroScaledBy, i));
    }
    if scale == 0 && !v.is_neg() && v.mag_le().len() == 1 && v.mag_le()[0] <= 10 {
        return bignum_static_element(ctx, BignumStatic::BdZeroThroughTen, v.mag_le()[0] as usize);
    }
    None
}

// -----------------------------------------------------------------------------
// Round 13 wave 13 (lane bigdec4; proposal BD3-3): the identity JDK 25 gives the
// results of the remaining native `valueOf` roads -- the `MathContext`
// overloads, exact `divide` and `toBigInteger`. Read road by road from the JDK
// 25.0.3 `java.math` sources; each helper names the road it follows. Switch
// `CRATONVM_BIGDECIMAL_MC_RESULT_CONSTANTS` (`W13_MC_RESULT_CONSTANTS`).
//
// The shape shared by the roads: the JDK's `doRound(long, ..)` and
// `doRound(BigInteger, ..)` end in `valueOf` (a constant for a small result),
// but `doRound(BigDecimal, mc)` answers its ARGUMENT when it drops no digit
// and a fresh `new BigDecimal(..)` when it does.
// -----------------------------------------------------------------------------

/// [`bd_constant_for`] under the wave-13 switch.
fn bd_mc_result_constant(
    ctx: &dyn NativeContext,
    v: &crate::bigint::BigInt,
    scale: i32,
) -> Option<ObjectRef> {
    if !w9_switch(ctx, W13_MC_RESULT_CONSTANTS) {
        return None;
    }
    bd_constant_for(ctx, v, scale)
}

/// `true` when rounding `x` (non-zero) to `precision` digits drops a digit:
/// JDK `doRound(BigDecimal, mc)` then answers a fresh `new BigDecimal`.
fn bd_mc_rounds(x: &crate::bigint::BigInt, precision: i32) -> bool {
    precision > 0 && !x.is_zero() && bigint_decimal_digit_count(x) > precision
}

/// What JDK 25's `add(augend, mc)` answers by identity for the result
/// `(q, s)` [`bd_add_mc_core`] computed, or `None` for a fresh object.
/// `negated_augend`: this is `subtract(subtrahend, mc)`, which is
/// `add(subtrahend.negate(), mc)`, so `ub` is the negated subtrahend and the
/// augend the JDK holds is `negate()`'s `valueOf(-c, scale, prec)`, not the
/// argument object.
///
/// Roads (`BigDecimal.java:1438`): `precision == 0` is `add(augend)` (every
/// small result a `valueOf`, wave 12); both operands zero is
/// `zeroValueOf(preferredScale)`; both non-zero is `doRound(BigInteger)`,
/// a `valueOf`. Exactly one zero: `result = doRound(x, mc)` -- `x` itself
/// unless a digit is dropped -- then `result` as is when its scale is the
/// preferred one, else `setScale` (`this` for an unchanged scale, a
/// `valueOf` for a raised one). A dropped digit leaves exactly `precision`
/// digits, so no raise follows it and the fresh object is the answer.
#[allow(clippy::too_many_arguments)]
fn bd_add_mc_identity(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
    (ua, sa): (&crate::bigint::BigInt, i32),
    (ub, sb): (&crate::bigint::BigInt, i32),
    precision: i32,
    (q, s): (&crate::bigint::BigInt, i32),
    negated_augend: bool,
) -> Option<ObjectRef> {
    if !w9_switch(ctx, W13_MC_RESULT_CONSTANTS) {
        return None;
    }
    match bd_add_mc_road((ua, sa), (ub, sb), precision, s, negated_augend) {
        BdMcAnswer::Constant => bd_constant_for(ctx, q, s),
        BdMcAnswer::Receiver => Some(this),
        BdMcAnswer::Operand => Some(other),
        BdMcAnswer::Fresh => None,
    }
}

/// Which object a JDK 25 `MathContext` road answers (BD3-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BdMcAnswer {
    /// A `valueOf`/`zeroValueOf`: the cached constant when the value has one.
    Constant,
    /// The receiver itself.
    Receiver,
    /// The operand argument itself.
    Operand,
    /// A fresh `new BigDecimal(..)`.
    Fresh,
}

/// [`bd_add_mc_identity`]'s decision, on values only (`s`: the result scale).
fn bd_add_mc_road(
    (ua, sa): (&crate::bigint::BigInt, i32),
    (ub, sb): (&crate::bigint::BigInt, i32),
    precision: i32,
    s: i32,
    negated_augend: bool,
) -> BdMcAnswer {
    if precision <= 0 || ua.is_zero() == ub.is_zero() {
        return BdMcAnswer::Constant;
    }
    let lhs_is_zero = ua.is_zero();
    let (x, xs) = if lhs_is_zero { (ub, sb) } else { (ua, sa) };
    if bd_mc_rounds(x, precision) {
        return BdMcAnswer::Fresh;
    }
    if s != xs {
        return BdMcAnswer::Constant;
    }
    match (lhs_is_zero, negated_augend) {
        (false, _) => BdMcAnswer::Receiver,
        (true, false) => BdMcAnswer::Operand,
        (true, true) => BdMcAnswer::Constant,
    }
}

/// `negate(mc)` is `negate().plus(mc)`: `negate()` is a `valueOf` for a
/// compact value, and `plus(mc)` answers it unless `doRound` drops a digit.
fn bd_negate_mc_constant(
    ctx: &dyn NativeContext,
    ua: &crate::bigint::BigInt,
    precision: i32,
    (q, s): (&crate::bigint::BigInt, i32),
) -> Option<ObjectRef> {
    if bd_mc_rounds(ua, precision) {
        return None;
    }
    bd_mc_result_constant(ctx, q, s)
}

/// `multiply(multiplicand, mc)` (`BigDecimal.java:1627`): every
/// `multiplyAndRound` road ends in `doRound(long|BigInteger, ..)` -- a
/// `valueOf` -- EXCEPT two compact operands whose product leaves `long`:
/// that road rounds in 128 bits and then calls `doRound(BigDecimal, mc)`, or
/// builds `new BigDecimal(..)` outright, and answers a fresh object for some
/// small results. That road keeps allocating here (the old behaviour).
fn bd_multiply_mc_constant(
    ctx: &dyn NativeContext,
    (ua, ub, prod): (&crate::bigint::BigInt, &crate::bigint::BigInt, &crate::bigint::BigInt),
    precision: i32,
    (q, s): (&crate::bigint::BigInt, i32),
) -> Option<ObjectRef> {
    // JDK compact: `|v| < 2^63` (`compactValFor`).
    let compact = |v: &crate::bigint::BigInt| v.magnitude_bits() <= 63;
    if precision > 0 && compact(ua) && compact(ub) && !compact(prod) {
        return None;
    }
    bd_mc_result_constant(ctx, q, s)
}

/// `divide(divisor, mc)` with `precision > 0` (`BigDecimal.java:1840`):
/// `zeroValueOf` for a zero dividend, else one of the four
/// `divide(.., preferredScale, mc)` bodies, each `quotient =
/// divideAndRound(..)` (a `valueOf`; an exact one first stripped of trailing
/// zeros down to `preferredScale`) and then `doRound(quotient, mc)`, which
/// drops a digit -- a fresh `new BigDecimal` -- only when the quotient has
/// `precision + 1` digits, i.e. is `10^precision` at some scale. Of the
/// constants only `1` (precision 1) and `10` (precision 2) at scale 0 can
/// come out of that, and for them the road is decided exactly:
/// * INEXACT `x / y` below the result: rounded up across a decade to
///   `10^precision`, then `doRound` -- fresh. At or above it: the quotient
///   already had `precision` digits -- the constant.
/// * EXACT: the stripped quotient's scale is at least `preferredScale`, so
///   for `preferredScale >= 1` it cannot be the scale-0 result, and `doRound`
///   produced it -- fresh (`new BigDecimal("1.0").divide(ONE, new
///   MathContext(1))`). For `preferredScale <= 0` the strip reaches scale 0
///   -- the constant.
fn bd_divide_mc_constant(
    ctx: &dyn NativeContext,
    (ua, sa): (&crate::bigint::BigInt, i32),
    (ub, sb): (&crate::bigint::BigInt, i32),
    precision: i32,
    (q, s): (&crate::bigint::BigInt, i32),
) -> Option<ObjectRef> {
    match bd_divide_mc_road((ua, sa), (ub, sb), precision, (q, s)) {
        BdMcAnswer::Constant => bd_mc_result_constant(ctx, q, s),
        _ => None,
    }
}

/// [`bd_divide_mc_constant`]'s decision, on values only.
fn bd_divide_mc_road(
    (ua, sa): (&crate::bigint::BigInt, i32),
    (ub, sb): (&crate::bigint::BigInt, i32),
    precision: i32,
    (q, s): (&crate::bigint::BigInt, i32),
) -> BdMcAnswer {
    let ambiguous = s == 0
        && !q.is_neg()
        && ((precision == 1 && q.mag_le() == &[1u32][..])
            || (precision == 2 && q.mag_le() == &[10u32][..]));
    if !ambiguous {
        return BdMcAnswer::Constant;
    }
    let preferred = i64::from(sa) - i64::from(sb);
    match bd_quotient_cmp(ua, sa, ub, sb, q) {
        Some(std::cmp::Ordering::Greater) => BdMcAnswer::Constant,
        Some(std::cmp::Ordering::Equal) if preferred <= 0 => BdMcAnswer::Constant,
        // Rounded up to it, an exact result `doRound` rescaled, or a
        // comparison too wide to be worth making: keep allocating.
        _ => BdMcAnswer::Fresh,
    }
}

/// `|ua * 10^-sa| / |ub * 10^-sb|` against a non-negative `v` at scale 0,
/// i.e. `|ua|` against `v * |ub| * 10^(sa - sb)`; `None` for a zero operand
/// or a scale gap out of all proportion to the operands (the quotient is
/// then nowhere near a one- or two-digit `v`, and the caller never asks).
fn bd_quotient_cmp(
    ua: &crate::bigint::BigInt,
    sa: i32,
    ub: &crate::bigint::BigInt,
    sb: i32,
    v: &crate::bigint::BigInt,
) -> Option<std::cmp::Ordering> {
    if ua.is_zero() || ub.is_zero() {
        return None;
    }
    let d = i64::from(sa) - i64::from(sb);
    let bound = i64::from(bigint_decimal_digit_count(ua))
        + i64::from(bigint_decimal_digit_count(ub))
        + 4;
    if d.abs() > bound {
        return None;
    }
    let lhs = ua.abs_value();
    let rhs = v.mul(&ub.abs_value());
    let (lhs, rhs) = if d >= 0 {
        (lhs, rhs.mul(&bigint_pow10(d as u32)))
    } else {
        (lhs.mul(&bigint_pow10(d.unsigned_abs() as u32)), rhs)
    };
    Some(lhs.cmp(&rhs))
}

fn native_bd_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // Both branches of `add(augend)` read `augend.intCompact` first.
        _ => {
            return bd_null_arg(
                ctx,
                "Cannot read field \"intCompact\" because \"augend\" is null",
                Value::Object(None),
            )
        }
    };
    // Exact: result scale = max(sa, sb); rescale both unscaled to it, then add.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = sa.max(sb);
    // `s - sa` in `i32` OVERFLOWS — see `bd_rescale_operand`.
    let sum = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa))?
        .add(&bd_rescale_operand(&ub, i64::from(s) - i64::from(sb))?);
    if let Some(c) = bd_result_constant(ctx, &sum, s) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &sum, s);
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_subtract(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return bd_null_arg(
                ctx,
                "Cannot read field \"intCompact\" because \"subtrahend\" is null",
                Value::Object(None),
            )
        }
    };
    // Exact: result scale = max(sa, sb); rescale both unscaled to it, then subtract.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = sa.max(sb);
    // `s - sa` in `i32` OVERFLOWS — see `bd_rescale_operand`.
    let diff = bd_rescale_operand(&ua, i64::from(s) - i64::from(sa))?
        .sub(&bd_rescale_operand(&ub, i64::from(s) - i64::from(sb))?);
    if let Some(c) = bd_result_constant(ctx, &diff, s) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &diff, s);
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `multiply`'s first line reads `multiplicand.scale`.
        _ => return bd_null_arg(ctx, BD_NPE_MULTIPLY, Value::Object(None)),
    };
    // Exact: result scale = checkScale(sa + sb); multiply the unscaled
    // integers directly. `sa + sb` used to be a bare `i32` addition of two
    // caller-chosen scales — see `bd_product_scale`.
    let (ua, sa) = bd_unscaled_bigint(ctx, this);
    let (ub, sb) = bd_unscaled_bigint(ctx, other);
    let s = bd_product_scale(&ua, sa, sb)?;
    let prod = ua.mul(&ub);
    if let Some(c) = bd_result_constant(ctx, &prod, s) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &prod, s);
    Ok(Some(Value::Object(Some(result?))))
}

/// `multiply`'s product scale — `this.checkScale((long) scale + multiplicand.scale)`,
/// JDK 25 `BigDecimal.multiply` (`BigDecimal.java:1655`) through `checkScale`
/// (`BigDecimal.java:4568-4578`).
///
/// # The addition has to happen in an `i64`, and the RECEIVER decides the refusal
///
/// `native_bd_multiply` computed `sa + sb` in `i32`. Both are caller-chosen
/// `BigDecimal` scales, so `bd(1,MAX).multiply(bd(1,1))` **overflowed**: a
/// panic in a debug build — and a Rust panic in a native is not a Java
/// throwable, it takes the VM down where HotSpot throws — and a silent wrap in
/// release, which then wrote a *negative* scale into the result and rendered
/// 2 GB of trailing zeros the first time anything printed it.
///
/// `checkScale` refuses only when the RECEIVER is non-zero:
///
/// ```java
///     int asInt = (int)val;
///     if (asInt != val) {
///         asInt = val > Integer.MAX_VALUE ? Integer.MAX_VALUE : Integer.MIN_VALUE;
///         BigInteger b;
///         if (intCompact != 0 && ((b = intVal) == null || b.signum() != 0))
///             throw new ArithmeticException(asInt > 0 ? "Underflow" : "Overflow");
///     }
///     return asInt;
/// ```
///
/// A zero receiver clamps to `Integer.MAX_VALUE`/`MIN_VALUE` and answers. This
/// is the same "the zero operand is exempt" shape F31-1 found for `add`, with
/// one difference that has to be transcribed rather than derived: it is the
/// **receiver**'s zeroness, not the product's. `bd(1,MAX).multiply(bd(0,MAX))`
/// throws even though the product is zero.
///
/// # MEASURED — `scratchpad/g10/Bd2Probe.java`, Temurin OpenJDK 25.0.3+9
///
/// ```text
/// bd(1,MAX).multiply(bd(1,MAX))    !! ArithmeticException: Underflow
/// bd(1,MAX).multiply(bd(1,1))      !! ArithmeticException: Underflow
/// bd(1,MAX).multiply(bd(0,MAX))    !! ArithmeticException: Underflow
/// bd(0,MAX).multiply(bd(1,MAX))     = unscaled=0 scale=2147483647
/// bd(0,MAX).multiply(bd(0,MAX))     = unscaled=0 scale=2147483647
/// bd(1,MAX).multiply(bd(1,0))       = unscaled=1 scale=2147483647
/// bd(1,MIN).multiply(bd(1,MIN))    !! ArithmeticException: Overflow
/// bd(1,MIN).multiply(bd(1,-1))     !! ArithmeticException: Overflow
/// bd(0,MIN).multiply(bd(1,MIN))     = unscaled=0 scale=-2147483648
/// bd(1,MAX).multiply(bd(1,MIN))     = unscaled=1 scale=-1
/// bd(1,2^30).multiply(bd(1,2^30))  !! ArithmeticException: Underflow
/// bd(0,2^30).multiply(bd(1,2^30))   = unscaled=0 scale=2147483647
/// ```
///
/// The two words are NOT interchangeable and neither is the sign: a sum past
/// `Integer.MAX_VALUE` is `"Underflow"` (the *value* underflows as the scale
/// grows) and a sum past `Integer.MIN_VALUE` is `"Overflow"`.
fn bd_product_scale(
    receiver_unscaled: &crate::bigint::BigInt,
    sa: i32,
    sb: i32,
) -> Result<i32, MethodCallFailed> {
    let sum = i64::from(sa) + i64::from(sb);
    if sum >= i64::from(i32::MIN) && sum <= i64::from(i32::MAX) {
        return Ok(sum as i32);
    }
    if receiver_unscaled.is_zero() {
        // `checkScale` clamps and returns; only a non-zero receiver throws.
        return Ok(if sum > i64::from(i32::MAX) {
            i32::MAX
        } else {
            i32::MIN
        });
    }
    if sum > i64::from(i32::MAX) {
        Err(bd_underflow())
    } else {
        Err(RuntimeError::ArithmeticException {
            message: "Overflow".to_string(),
        }
        .into())
    }
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
    // EXACT, through the shipping `bd_exact_divide_core`. This body used to
    // divide in `f64`, which was wrong twice: it lost precision on every
    // division, and it RETURNED `0.3333333333333333` for `1/3` where
    // `BigDecimal.divide(BigDecimal)` must throw `ArithmeticException`
    // ("Non-terminating decimal expansion"). The core raises that, and the
    // divide-by-zero one, exactly as the shipping body does.
    let (ua, ea) = bd_unscaled_bigint(ctx, this);
    let (ub, eb) = bd_unscaled_bigint(ctx, other);
    let (q, scale) = bd_exact_divide_core(&ua, ea, &ub, eb)?;
    // Store by the layout's convention: rendered for display when scale >= 0
    // (the '.' adds no digit, so `bd_unscaled_bigint` still reads the digits),
    // bare unscaled digits when scale < 0 -- rendering would append zeros and
    // make the stored digits disagree with the stored scale.
    let digits = q.to_decimal();
    let stored = if scale >= 0 {
        apply_scale(&digits, scale)
    } else {
        digits
    };
    let result = bd_alloc(ctx, &stored, scale);
    Ok(Some(Value::Object(Some(result?))))
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
    // `divide(BigDecimal, int, int)`'s third argument is the rounding mode; it
    // was read by nobody, so every negative-scale and every rounding decision
    // below used whatever `format!` does.
    let mode = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 4, // ROUND_HALF_UP
    };
    let a: f64 = bd_read_unchecked(ctx, this).parse().unwrap_or(0.0);
    let b: f64 = bd_read_unchecked(ctx, other).parse().unwrap_or(0.0);
    if b == 0.0 {
        return Err(RuntimeError::ArithmeticException {
            message: "BigDecimal divide by zero".to_string(),
        }
        .into());
    }
    // HAZARD (sign-losing `as` cast, fixed 2026-08-13): `scale` may legally be
    // NEGATIVE — `x.divide(y, -2, HALF_UP)` means "round to hundreds" and is an
    // ordinary BigDecimal call. `prec = new_scale as usize` turned -2 into
    // 18_446_744_073_709_551_614 and `format!("{:.prec$}")` then tried to render
    // that many fractional digits: an unbounded allocation reachable from plain
    // bytecode, i.e. a denial of service, not a wrong answer. Format at a
    // non-negative precision and let the existing exact `setScale` machinery
    // apply the negative scale with the caller's rounding mode.
    let work_prec = new_scale.max(0);
    let s = format!("{:.prec$}", a / b, prec = work_prec as usize);
    if new_scale >= 0 {
        let result = bd_alloc(ctx, &s, new_scale);
        return Ok(Some(Value::Object(Some(result?))));
    }
    let interim = bd_alloc(ctx, &s, 0)?;
    // `bd_set_scale_impl` allocates, so `interim` can be relocated by a minor
    // GC before it is read — the same use-after-move `bi_alloc` documents.
    let h = ctx.pin_native_root(interim);
    let interim = ctx.read_native_pin(h, interim);
    let result = bd_set_scale_impl(ctx, interim, new_scale, mode);
    ctx.unpin_native_roots(h);
    result
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
    // EXACT, through the same `bd_compare_unscaled` the shipping
    // `native_bd_compare_to_exact` uses. This synthetic body used to parse both
    // operands to `f64` and compare those, which ranks any two values agreeing
    // in their leading ~17 significant digits as EQUAL -- the shape a Newton
    // bisection produces near convergence. `compareTo` ignores scale
    // (`2.0` and `2.00` compare equal), so an exact VALUE is all it needs.
    // Operands through `bd_unscaled_bigint`, which honours the stored scale --
    // so an extreme-scale value compares as what it is.
    let (ua, ea) = bd_unscaled_bigint(ctx, this);
    let (ub, eb) = bd_unscaled_bigint(ctx, other);
    Ok(Some(Value::Int(
        match bd_compare_unscaled(&ua, ea, &ub, eb) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
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
    let a = bd_read_unchecked(ctx, this);
    let b = bd_read_unchecked(ctx, other);
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
    // `CRATONVM_BIGDECIMAL_TOSTRING_CACHE=0` fallback only: restores interning
    // (see `native_bi_to_string`).
    let java_str = ctx.intern_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// `BigDecimal.toPlainString()` — never uses exponential notation.
fn native_bd_to_plain_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = bd_read(ctx, this)?;
    // `CRATONVM_BIGNUM_STRINGS_UNINTERNED=0` fallback only (see
    // `native_bi_to_string`).
    let java_str = ctx.intern_string(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// `native_bd_to_plain_string` with a fresh, uninterned result; see
/// `native_bi_to_string_uninterned` for why (`CRATONVM_BIGNUM_STRINGS_UNINTERNED`).
fn native_bd_to_plain_string_uninterned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let s = bd_read(ctx, this)?;
    let java_str = ctx.create_string_uninterned(&s);
    Ok(Some(Value::Object(Some(java_str))))
}

/// The unscaled value with the fraction dropped (truncation toward zero), with
/// **no refusal of any kind**.
///
/// This is the arithmetic only. Whether a given `scale` is allowed to reach it
/// is a question with THREE different JDK answers depending on which method the
/// caller is serving, and each caller answers it before calling here:
/// [`bd_truncated_bigint`] for `toBigInteger()`, and
/// [`bd_narrowing_truncates_to_zero`] for `intValue()`/`longValue()`.
///
/// `10^n` is `(5^n) << n` — `BigInteger.pow` factors `2^getLowestSetBit()` out
/// of the base, so that is not an approximation of `TEN.pow(n)`, it is
/// `TEN.pow(n)`. The divisor used to be built as the literal decimal string
/// `"1" + "0"*scale` and handed to `BigInt::from_decimal`: a `scale`-byte
/// `String` plus an O(n²) digit-at-a-time parse, from an argument.
fn bd_truncate(u: &crate::bigint::BigInt, scale: i32) -> crate::bigint::BigInt {
    if scale == 0 || u.is_zero() {
        return u.clone();
    }
    if scale < 0 {
        // `scale.unsigned_abs()`, NOT `-scale`: `-i32::MIN` is a debug panic
        // and a release wrap, and a Rust panic is not a Java throwable. Both
        // callers already refuse the sizes that can reach here, but this stays
        // total so a third caller cannot reintroduce the abort.
        return u.mul(&bigint_pow10(scale.unsigned_abs()));
    }
    u.div(&bigint_pow10(scale as u32))
}

/// `10^n` as a limb `BigInt`. See [`bigint_mul_pow10`] for why this is
/// `5^n << n` and not an `n`-byte decimal string.
fn bigint_pow10(n: u32) -> crate::bigint::BigInt {
    bigint_pow5_memo(n).shl(n)
}

/// `bi * 10^n` with `5^n` from the per-thread memo: `lib.rs`
/// `bigint_mul_pow10`'s body (round 14 wave 2, lane bigdec; proposal BD4-2),
/// which rebuilt `5^n` by square-and-multiply on every `add`/`subtract`
/// rescale. `(bi * 5^n) << n` is `bi * (5^n << n)` exactly (the shift is a
/// multiplication by `2^n` for either sign) and skips the shifted copy of
/// the power. `CRATONVM_BIGNUM_POW5_CACHE=0` still computes the power afresh.
pub(crate) fn bigint_mul_pow10_memo(bi: &crate::bigint::BigInt, n: u32) -> crate::bigint::BigInt {
    if n == 0 || bi.is_zero() {
        return bi.clone();
    }
    bi.mul(&bigint_pow5_memo(n)).shl(n)
}

// Round 13 wave 13 (lane bigdec4; proposal BD3-2): `5^n` memo, the JDK's
// `BIG_TEN_POWERS_TABLE` idea. `bigint_pow5` rebuilt the power by
// square-and-multiply on every `new BigDecimal(double)` (n up to 1074), every
// binary `doubleValue`/`floatValue` of a positive scale, every wide digit
// count and every `MathContext` rounding (`bigint_pow10(drop)`); a loop over
// values of one shape rebuilt the same ~80-limb power each call.
//
// Per THREAD, not per VM, and deliberately: the entries are pure values
// (`5^n` is the same number in every VM), so a VM key would only throw
// valid entries away, and nothing here is compatibility state. It is not a
// process global either (no lock, no sharing). Bounded: `POW5_MEMO_SLOTS`
// direct-mapped slots of `n <= POW5_MEMO_MAX_N` (a `5^2200` is 160 limbs),
// so at most ~40 KB per thread that ever formats a `BigDecimal`.
// `CRATONVM_BIGNUM_POW5_CACHE=0` computes every power afresh (the old road).
const POW5_MEMO_SLOTS: usize = 64;
const POW5_MEMO_MAX_N: u32 = 2200;

struct Pow5Memo {
    /// The switch, read once per thread on first use.
    on: Option<bool>,
    slots: Vec<Option<(u32, std::rc::Rc<crate::bigint::BigInt>)>>,
}

thread_local! {
    static POW5_MEMO_TLS: std::cell::RefCell<Pow5Memo> = std::cell::RefCell::new(Pow5Memo {
        on: None,
        slots: Vec::new(),
    });
}

/// `5^n`, from the per-thread memo when it holds it (see `POW5_MEMO_SLOTS`).
fn bigint_pow5_memo(n: u32) -> std::rc::Rc<crate::bigint::BigInt> {
    use std::rc::Rc;
    POW5_MEMO_TLS.with(|cell| {
        let Ok(mut memo) = cell.try_borrow_mut() else {
            // Not reachable (nothing below re-enters), but never a panic.
            return Rc::new(bigint_pow5(n));
        };
        let on = *memo.on.get_or_insert_with(|| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGNUM_POW5_CACHE")
        });
        bigint_pow5_memo_in(&mut memo, on, n)
    })
}

/// [`bigint_pow5_memo`]'s body over an explicit memo and switch (the unit
/// tests drive it directly, both arms).
fn bigint_pow5_memo_in(memo: &mut Pow5Memo, on: bool, n: u32) -> std::rc::Rc<crate::bigint::BigInt> {
    use std::rc::Rc;
    if !on {
        return Rc::new(bigint_pow5(n));
    }
    if n <= 27 {
        // 5^27 < 2^63: two limbs, no multiplication.
        let v = 5u64.pow(n);
        return Rc::new(crate::bigint::BigInt::from_le_words(
            false,
            vec![v as u32, (v >> 32) as u32],
        ));
    }
    if n > POW5_MEMO_MAX_N {
        return Rc::new(bigint_pow5(n));
    }
    if memo.slots.len() != POW5_MEMO_SLOTS {
        memo.slots = vec![None; POW5_MEMO_SLOTS];
    }
    let slot = &mut memo.slots[n as usize % POW5_MEMO_SLOTS];
    if let Some((k, v)) = slot {
        if *k == n {
            return Rc::clone(v);
        }
    }
    let v = Rc::new(bigint_pow5(n));
    *slot = Some((n, Rc::clone(&v)));
    v
}

/// `BigDecimal.toBigInteger()`'s refusal — and ONLY `toBigInteger()`'s.
///
/// `toBigInteger()` is `setScale(0, ROUND_DOWN).inflated()`
/// (`BigDecimal.java:3515`), so it inherits `setScale`'s two guards in order:
/// the **instance** `checkScale` (`BigDecimal.java:4568`), which CLAMPS the
/// out-of-`int` difference to `Integer.MAX_VALUE` and therefore reports
/// `"Underflow"` — the opposite sign from `toPlainString`'s casting
/// `checkScaleNonZero`, see [`bd_plain_string_check`] — and then
/// `bigTenToThe`, i.e. [`bd_pow_ten_check`].
///
/// Both guards are skipped for a zero value: `setScale`'s third line is
/// `if (this.signum() == 0) return zeroValueOf(newScale);`.
///
/// # MEASURED — and note the intValue/longValue rows, which DISAGREE
///
/// `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`, `scratchpad/f31/{Bd,Bd2}.java`:
///
/// ```text
/// bd(1,MIN).toBigInteger()          !! ArithmeticException: Underflow                      [0 ms]
/// bd(-1,MIN).toBigInteger()         !! ArithmeticException: Underflow                      [0 ms]
/// bd(1,MIN+1).toBigInteger()        !! ArithmeticException: BigInteger would overflow…     [0 ms]
/// bd(1,-715827883).toBigInteger()   !! ArithmeticException: BigInteger would overflow…     [0 ms]
/// bd(1,MAX).toBigInteger()          !! ArithmeticException: BigInteger would overflow…     [0 ms]
/// bd(1,715827883).toBigInteger()    !! ArithmeticException: BigInteger would overflow…     [0 ms]
/// bd(0,MIN).toBigInteger()           = 0                                                   [0 ms]
/// bd(0,715827883).toBigInteger()     = 0                                                   [0 ms]
/// bd(0,MAX).toBigInteger()           = 0                                                   [0 ms]
/// bd(1,-3).toBigInteger()            = 1000                                                [1 ms]
/// bd(123456,5).toBigInteger()        = 1                                                   [0 ms]
///
/// bd(1,MIN).intValue()               = 0      bd(1,MIN).longValue()  = 0                   [0 ms]
/// bd(1,MAX).longValue()              = 0      bd(1,715827883).longValue() = 0              [0 ms]
/// ```
///
/// The last two rows are the trap this predicate exists to avoid: the very same
/// `(unscaled, scale)` pairs that `toBigInteger()` REFUSES are answered `0` by
/// `intValue()`/`longValue()` in under a millisecond. A guard placed in the
/// shared helper would be a fresh divergence on the narrowing conversions —
/// worse than the DoS it removes. The positive-scale road refuses too
/// (`bd(1,MAX)`), because `setScale(0)` builds `bigTenToThe(drop)` as a
/// divisor; only the *zero* value escapes it.
fn bd_to_big_integer_check(u: &crate::bigint::BigInt, scale: i32) -> Result<(), MethodCallFailed> {
    // `newScale == oldScale` returns `this`; `signum() == 0` takes any scale.
    if scale == 0 || u.is_zero() {
        return Ok(());
    }
    if scale < 0 {
        // `int raise = checkScale((long) 0 - oldScale);` — the CLAMPING form.
        let raise = -i64::from(scale);
        if raise > i64::from(i32::MAX) {
            return Err(bd_underflow());
        }
        return bd_pow_ten_check(raise as i32);
    }
    // `int drop = checkScale((long) oldScale - 0);` always fits, so the only
    // guard left on this road is `bigTenToThe(drop)`.
    bd_pow_ten_check(scale)
}

/// `ArithmeticException("Underflow")` — JDK 25 `BigDecimal.checkScale`
/// (`BigDecimal.java:4568-4578`) and `checkScale(BigInteger,long)`
/// (`BigDecimal.java:4692-4700`). Both clamp a too-large positive difference to
/// `Integer.MAX_VALUE` first, so `asInt > 0` and the word is "Underflow".
fn bd_underflow() -> MethodCallFailed {
    RuntimeError::ArithmeticException {
        message: "Underflow".to_string(),
    }
    .into()
}

/// One operand of `add`/`subtract`, raised to the common scale — with the
/// alignment guard the JDK applies at the same point.
///
/// # `raise` is an `i64` because the `i32` subtraction OVERFLOWS
///
/// `BigDecimal.add`'s first line is `long sdiff = (long) scale1 - scale2;`
/// (`BigDecimal.java:5247`, and identically in the other two `add` overloads) —
/// a `long`, deliberately. This file computed `s - sa` in `i32`:
/// `new BigDecimal(ONE, MIN).add(new BigDecimal(ONE, MAX))` made that
/// `MAX - MIN`, a debug panic and a release wrap to `-1`, and `-1` reaches
/// `bigint_mul_pow10`'s `n <= 0` arm, so the release build answered with the
/// operand UNSCALED. That is a wrong answer, not only a DoS.
///
/// # A ZERO operand is exempt, and that exemption is not optional
///
/// `checkScale` throws only `if (intCompact != 0 …)` / `if (intVal.signum() != 0)`,
/// and `longMultiplyPowerTen` returns `val` unchanged when `val == 0`. So a
/// zero operand is never scaled and never refused, however large the raise. The
/// operand that gets raised is the one with the SMALLER scale; the other one's
/// raise is 0.
///
/// # MEASURED — `scratchpad/f31/{Bd,Bd2,Bd4}.java`, OpenJDK 25.0.3+9
///
/// ```text
/// bd(1,MIN).add(bd(1,MAX))            !! ArithmeticException: Underflow                    [0 ms]
/// bd(1,MAX).add(bd(1,MIN))            !! ArithmeticException: Underflow                    [0 ms]
/// bd(1,MAX).subtract(bd(1,MIN))       !! ArithmeticException: Underflow                    [0 ms]
/// bd(1,MIN).add(bd(0,MAX))            !! ArithmeticException: Underflow                    [0 ms]
/// bd(0,0).add(bd(1,MIN))              !! ArithmeticException: Underflow                    [0 ms]
/// bd(0,MAX).add(bd(1,MIN))            !! ArithmeticException: Underflow                    [0 ms]
/// bd(1,0).add(bd(1,715827883))        !! ArithmeticException: BigInteger would overflow…   [0 ms]
/// bd(1,0).add(bd(0,715827883))        !! ArithmeticException: BigInteger would overflow…   [0 ms]
/// bd(0,715827883).add(bd(1,0))        !! ArithmeticException: BigInteger would overflow…   [0 ms]
/// bd(0,MIN).add(bd(1,MAX))             = 1E-2147483647                                     [0 ms]
/// bd(0,0).add(bd(1,MAX))               = 1E-2147483647                                     [0 ms]
/// bd(1,MAX).add(bd(0,0))               = 1E-2147483647                                     [0 ms]
/// bd(0,0).add(bd(1,715827883))         = 1E-715827883                                     [57 ms]
/// bd(0,0).subtract(bd(1,715827883))    = -1E-715827883                                     [0 ms]
/// bd(1,2).add(bd(3,1))                 = 0.31       bd(1,2).subtract(bd(3,1)) = -0.29       [0 ms]
/// ```
///
/// Rows 4–6 and rows 10–13 are the pair that fixes the shape of this function:
/// it is the *raised* operand's zeroness that exempts, not either operand's.
/// `bd(1,MIN).add(bd(0,MAX))` throws (the raised operand is the `1`) while
/// `bd(0,MIN).add(bd(1,MAX))` does not (the raised operand is the `0`), and
/// they differ in nothing else. A guard that tested the raise alone would
/// refuse four of these rows where HotSpot answers.
fn bd_rescale_operand(
    u: &crate::bigint::BigInt,
    raise: i64,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    if raise <= 0 {
        return Ok(u.clone());
    }
    if u.is_zero() {
        return Ok(crate::bigint::BigInt::zero());
    }
    if raise > i64::from(i32::MAX) {
        return Err(bd_underflow());
    }
    bd_pow_ten_check(raise as i32)?;
    Ok(bigint_mul_pow10(u, raise as i32))
}

/// The unscaled value with the fraction dropped — the integer part
/// `BigDecimal.toBigInteger()` returns, and its refusals.
///
/// FALLIBLE since 2026-08-13 (F20-1 N1c). It is no longer shared with
/// `intValue()`/`longValue()`, which take [`bd_narrowing_truncates_to_zero`]
/// first: the JDK answers those two differently, and by a wide margin — see
/// [`bd_to_big_integer_check`]'s transcript.
fn bd_truncated_bigint(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<crate::bigint::BigInt, MethodCallFailed> {
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    bd_to_big_integer_check(&u, scale)?;
    Ok(bd_truncate(&u, scale))
}

/// `BigDecimal.longValue()`'s three fast paths (`BigDecimal.java:3553-3571`),
/// which is where `intValue()`/`longValue()` differ from `toBigInteger()`.
///
/// ```java
/// if (this.signum() == 0 || fractionOnly() || scale <= -64) { return 0; }
/// else { return toBigInteger().longValue(); }
/// ```
///
/// Returning `true` here means "the JDK answers 0 without ever building
/// `10^scale`". Returning `false` means the remaining division IS reachable —
/// and, crucially, is then bounded by the OPERAND rather than by the argument,
/// which is why `intValue`/`longValue` need no refusal at all.
///
/// # The `fractionOnly` bound is deliberately an UPPER bound on the precision
///
/// `fractionOnly()` is `precision() <= scale`, i.e. `|value| < 1`. Computing
/// `precision()` exactly costs the decimal conversion this lane is trying to
/// avoid, so this uses `bits/3 + 1 >= digits` (safe because `log10(2) < 1/3`).
/// An upper bound can only make the fast path fire LESS often than the JDK's,
/// never more — and when it does not fire, the fall-through divides by
/// `10^scale` with `scale < bits/3 + 1`, which produces the same `0` and is
/// bounded by the operand's own size. So the answer is identical and the
/// allocation is no longer argument-driven either way.
///
/// # MEASURED — `scratchpad/f31/{Bd,Bd4}.java`, OpenJDK 25.0.3+9
///
/// ```text
/// bd(1,MIN).longValue()   = 0    bd(1,MIN).intValue()  = 0    bd(0,MIN).longValue() = 0
/// bd(1,MAX).longValue()   = 0    bd(1,MAX).intValue()  = 0    bd(1,715827883).longValue() = 0
/// bd(1,-65).longValue()   = 0    bd(7,-64).longValue() = 0
/// bd(1,-64).longValue()   = 0    bd(7,-63).longValue() = -9223372036854775808
/// bd(1,-63).longValue()   = -9223372036854775808       bd(1,-63).intValue() = 0
/// bd(1,-1).longValue()    = 10   bd(-7,-3).longValue() = -7000
/// bd(1,5).longValue()     = 0    bd(123456,5).longValue()  = 1   bd(-123456,5).longValue() = -1
/// bd(123456,6).longValue()= 0    bd(123456,7).longValue()  = 0   bd(-123456,5).intValue()  = -1
/// bd(3,-20).intValue()    = 691011584   bd(3,-20).longValue() = 4852094820647174144
/// bd(1,-33).intValue()    = 0
/// new BigDecimal(TEN.pow(30),30).longValue() = 1   …31).longValue() = 0
/// new BigDecimal(TEN.pow(30),-2).longValue() = -8814407033341083648
/// ```
///
/// `scale <= -64` is not an optimisation: `10^64` has 64 factors of two, so all
/// 64 bits of the `long` are zero. `bd(7,-63)` vs `bd(7,-64)` is that line.
fn bd_narrowing_truncates_to_zero(u: &crate::bigint::BigInt, scale: i32) -> bool {
    if u.is_zero() || scale <= -64 {
        return true;
    }
    if scale > 0 {
        // `fractionOnly()`, with an upper bound for `precision()`.
        let digits_upper = u.magnitude_bits() / 3 + 1;
        if u64::from(scale as u32) >= digits_upper {
            return true;
        }
    }
    false
}

fn native_bd_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // JDK narrowing conversion: `(int) longValue()` — the low 32
    // two's-complement bits of the truncated value. The old f64 path both
    // saturated (f64→i32 casts clamp) and lost precision past 2^53.
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    if bd_narrowing_truncates_to_zero(&u, scale) {
        return Ok(Some(Value::Int(0)));
    }
    let t = bd_truncate(&u, scale);
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
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    if bd_narrowing_truncates_to_zero(&u, scale) {
        return Ok(Some(Value::Long(0)));
    }
    let t = bd_truncate(&u, scale);
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
    let t = bd_truncated_bigint(ctx, this)?;
    if let Some(o) = bd_to_big_integer_identity(ctx, this, &t) {
        return Ok(Some(Value::Object(Some(o))));
    }
    let obj = bi_alloc_int(ctx, &t);
    Ok(Some(Value::Object(Some(obj?))))
}

/// Round 13 wave 13 (lane bigdec4; BD3-3): the object JDK 25's `toBigInteger()`
/// answers when it is not a fresh `BigInteger`. The body is
/// `setScale(0, ROUND_DOWN).inflated()`: at scale 0 `setScale` is `this` and
/// `inflated()` answers the receiver's own `intVal` when it holds one;
/// otherwise every road of `setScale` that can give a value in `-16..=16`
/// ends in `valueOf`/`zeroValueOf` (whose `ZERO_THROUGH_TEN` entries hold
/// `BigInteger.ZERO`/`ONE`/`TWO`/`valueOf(3..9)`/`TEN`) or in an `intVal`-less
/// object, and `inflated()` is then `BigInteger.valueOf(intCompact)` -- the
/// JDK's `ZERO`/`posConst`/`negConst`. HotSpot:
/// `new BigDecimal("1.5").toBigInteger() == BigInteger.ONE`.
fn bd_to_big_integer_identity(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    t: &crate::bigint::BigInt,
) -> Option<ObjectRef> {
    if !w9_switch(ctx, W13_MC_RESULT_CONSTANTS) {
        return None;
    }
    let (iv, sc, _, _) = bd_layout(ctx)?;
    if let Value::Int(0) = ctx.get_field(this, sc) {
        if let Value::Object(Some(o)) = ctx.get_field(this, iv) {
            return Some(o);
        }
    }
    let mag = t.mag_le();
    let m = match mag {
        [] => 0usize,
        [w] if *w <= 16 => *w as usize,
        _ => return None,
    };
    if m == 0 {
        bignum_static(ctx, BignumStatic::BiZero)
    } else if t.is_neg() {
        bignum_static_element(ctx, BignumStatic::BiNegConst, m)
    } else {
        bignum_static_element(ctx, BignumStatic::BiPosConst, m)
    }
}

/// `(unscaled, scale)` as a `f64` — **without rendering the value**.
///
/// # This closes F31-1 §10's first residual, on the one caller that ships
///
/// `native_bd_double_value` was `bd_read_unchecked(ctx, this).parse()`.
/// `bd_read_unchecked` is `apply_scale`, which for a negative scale appends
/// `scale.unsigned_abs()` literal `'0'` characters: `bd(1, Integer.MIN_VALUE)`
/// asked for a 2_147_483_649-byte `String` from one ordinary
/// `doubleValue()` call and then handed it to a decimal parser. HotSpot answers
/// that same call `Infinity` in 0 ms — it never renders anything
/// (`BigDecimal.doubleValue`, `BigDecimal.java:3600-3641`). So this cannot be
/// closed by refusing; it closes by not rendering, and the rendering that
/// remains here is bounded by the OPERAND's digit count, never by the argument.
///
/// # Why a string round-trip at all, and why it is exact
///
/// `unscaled × 10^-scale` = `0.<digits> × 10^(len(digits) - scale)`, so the
/// decimal handed to the parser is the exact value and Rust's `f64` parser is
/// correctly rounded — the same round-to-nearest-even
/// `Double.parseDouble(this.toString())` performs on the JDK's own fall-through
/// path. The two clamps below exist only so the exponent handed to the parser
/// cannot be `-i64::from(i32::MIN)`-shaped: they are *proved*, not tuned.
/// `adjusted` is the decimal exponent of the leading digit.
///
/// * `adjusted >= 309` ⇒ `|v| >= 10^309 > f64::MAX (1.797e308)` ⇒ `±Infinity`.
/// * `adjusted <= -325` ⇒ `|v| < 10^-324 < 2.47e-324`, half of the smallest
///   subnormal (`4.9e-324`) ⇒ `±0.0`.
///
/// Everything in between leaves the parser an exponent in `-324..=309`.
///
/// # MEASURED — `scratchpad/g10/Bd2Probe.java`, Temurin OpenJDK 25.0.3+9
///
/// ```text
/// bd(1,MIN)   = Infinity      bd(-1,MIN)  = -Infinity     bd(0,MIN)  = 0.0
/// bd(1,-309)  = Infinity      bd(1,-308)  = 1.0E308       bd(15,-308) = Infinity
/// bd(1,MAX)   = 0.0           bd(-1,MAX)  = -0.0          bd(0,MAX)  = 0.0
/// bd(1,323)   = 9.9E-324      bd(1,324)   = 0.0           bd(-1,324) = -0.0
/// bd(15,324)  = 1.5E-323      bd(49,326)  = 0.0           bd(1,325)  = 0.0
/// bd(17976931348623157,-292) = 1.7976931348623157E308
/// bd(17976931348623159,-292) = Infinity
/// bd(9007199254740993,0)     = 9.007199254740992E15   (round-to-even, not truncation)
/// ```
///
/// The `bd(1,323)` / `bd(1,324)` pair is the one that pins the low clamp at
/// `-325` and not `-324`: `1E-324` still rounds to zero, but the *bound* has to
/// admit it so that `bd(15,324)` (same `adjusted`, different answer) is
/// computed rather than clamped.
fn bd_to_f64(u: &crate::bigint::BigInt, scale: i32) -> f64 {
    if u.is_zero() {
        // MEASURED: every `bd(0, s)` is `0.0`, never `-0.0`, at every scale.
        return 0.0;
    }
    let dec = u.to_decimal();
    let (neg, digits) = match dec.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, dec.as_str()),
    };
    // `i64::from(scale)`, never `-scale`: `-i32::MIN` is the shape this whole
    // family exists to remove.
    let e10 = digits.len() as i64 - i64::from(scale); // v = 0.<digits> * 10^e10
    let adjusted = e10 - 1; // decimal exponent of the leading digit
    if adjusted >= 309 {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if adjusted <= -325 {
        return if neg { -0.0 } else { 0.0 };
    }
    let sign = if neg { "-" } else { "" };
    let text = format!("{sign}0.{digits}E{e10}");
    text.parse::<f64>().unwrap_or(0.0)
}

/// The same value as an `f32`, parsed at `f32` precision rather than rounded
/// twice through an `f64` — `BigDecimal.floatValue()`'s general road is
/// `Float.parseFloat(this.toString())` (`BigDecimal.java:3573-3597`), and
/// double rounding is observable.
///
/// The clamps are `f32`'s: `f32::MAX` is `3.4e38` so `adjusted >= 39` is
/// `±Infinity`, and half the smallest subnormal is `7.006e-46`, so
/// `adjusted <= -47` (`|v| < 10^-46`) is `±0.0`. It was `<= -46`, which zeroed
/// `[7.006e-46, 1e-45)` -- values HotSpot rounds up to `Float.MIN_VALUE`
/// (`8.859721131365011933E-46` -> `1.4E-45`, found by round 13 wave 12's
/// binary-road differential).
///
/// MEASURED — `bd(1,-308).floatValue() = Infinity` while its `doubleValue()` is
/// `1.0E308`; `bd(-1,308).floatValue() = -0.0`; `bd(9007199254740993,1)` is
/// `9.0071994E14` as a float and `9.007199254740992E14` as a double.
fn bd_to_f32(u: &crate::bigint::BigInt, scale: i32) -> f32 {
    if u.is_zero() {
        return 0.0;
    }
    let dec = u.to_decimal();
    let (neg, digits) = match dec.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, dec.as_str()),
    };
    let e10 = digits.len() as i64 - i64::from(scale);
    let adjusted = e10 - 1;
    if adjusted >= 39 {
        return if neg {
            f32::NEG_INFINITY
        } else {
            f32::INFINITY
        };
    }
    if adjusted <= -47 {
        return if neg { -0.0 } else { 0.0 };
    }
    let sign = if neg { "-" } else { "" };
    let text = format!("{sign}0.{digits}E{e10}");
    text.parse::<f32>().unwrap_or(0.0)
}

// =============================================================================
// Round 13 wave 12 (lane bigdec3): `doubleValue()` / `floatValue()` in binary
// (proposals R13C-4 of `jit-r13-callcost-proposals-RETIRED-20260929.md` and BD2-3 of
// `jit-r13-bigdec2-proposals-RETIRED-20260929.md`). Switch `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE`.
//
// `bd_to_f64`/`bd_to_f32` above render the whole unscaled value to decimal
// (`BigInt::to_decimal`, O(words^2)) and re-parse it. Both are correctly
// rounded (round to nearest, ties to even, of the EXACT value), which is what
// JDK 25's `doubleValue`/`floatValue` answer too: the compact fast path is one
// IEEE operation on exact operands, and `fullDoubleValue`/`fullFloatValue`
// round the exact value once. Correct rounding has one answer, so the binary
// road below returns bit-for-bit what the string road returns; the
// `#[cfg(test)]` differential in `r13w12_bigdec3_tests` holds it to that.
// =============================================================================

/// An IEEE-754 binary interchange format, for [`bd_round_binary`].
struct BdBinFormat {
    /// Significand bits including the implicit leading one (53 / 24).
    p: u32,
    /// Exponent of the smallest subnormal (`2^-1074` / `2^-149`).
    min_lsb: i64,
    /// Exponent bias (1023 / 127).
    bias: i64,
    /// Biased exponent field of Infinity (2047 / 255).
    inf_biased: u64,
    /// A value whose `log10` is certainly above this is `Infinity`
    /// (`log10(MAX)` is 308.25 / 38.53; margin for the estimate's error).
    inf_log10: f64,
    /// A value whose `log10` is certainly below this rounds to zero (half the
    /// smallest subnormal is `10^-323.6` / `10^-45.15`; same margin).
    zero_log10: f64,
}

const BD_BINARY64: BdBinFormat = BdBinFormat {
    p: 53,
    min_lsb: -1074,
    bias: 1023,
    inf_biased: 2047,
    inf_log10: 309.5,
    zero_log10: -325.5,
};

const BD_BINARY32: BdBinFormat = BdBinFormat {
    p: 24,
    min_lsb: -149,
    bias: 127,
    inf_biased: 255,
    inf_log10: 39.5,
    zero_log10: -46.5,
};

/// Exact powers of ten as `f64` (`5^22 < 2^53`), the JDK's `DOUBLE_10_POW`.
const BD_F64_POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Exact powers of ten as `f32` (`5^10 < 2^24`), the JDK's `FLOAT_10_POW`.
const BD_F32_POW10: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];

/// The low 64 bits of a magnitude.
fn bd_mag_low_u64(m: &crate::bigint::BigInt) -> u64 {
    let w = m.mag_le();
    u64::from(w.first().copied().unwrap_or(0)) | (u64::from(w.get(1).copied().unwrap_or(0)) << 32)
}

/// Bit `n` of a little-endian magnitude.
fn bd_mag_bit(words: &[u32], n: u64) -> bool {
    let Ok(i) = usize::try_from(n / 32) else {
        return false;
    };
    words.get(i).is_some_and(|&w| (w >> (n % 32)) & 1 == 1)
}

/// Whether any of the bits `[0, n)` of a little-endian magnitude is set.
fn bd_mag_low_bits_nonzero(words: &[u32], n: u64) -> bool {
    let full = usize::try_from(n / 32).unwrap_or(usize::MAX);
    if words.iter().take(full).any(|&w| w != 0) {
        return true;
    }
    let rem = (n % 32) as u32;
    rem != 0 && words.get(full).is_some_and(|&w| w & ((1u32 << rem) - 1) != 0)
}

/// The raw bits (sign clear) of `(m + f) * 2^e` rounded to nearest, ties to
/// even, in format `fmt`, where `m > 0` is an integer and `0 <= f < 1` with
/// `f > 0` exactly when `sticky`. Callers guarantee that `m` has at least
/// `p + 2` bits whenever `sticky` is set, so the rounding bit and at least one
/// bit below it are real bits of `m` and `sticky` only ever breaks a tie.
fn bd_round_binary(m: &crate::bigint::BigInt, e: i64, sticky: bool, fmt: &BdBinFormat) -> u64 {
    let l = m.magnitude_bits() as i64;
    if l == 0 {
        return 0;
    }
    let p = i64::from(fmt.p);
    let top = l - 1 + e; // exponent of the leading bit
    let mut lsb = (top - (p - 1)).max(fmt.min_lsb); // exponent of the result's last bit
    let shift = lsb - e; // low bits of `m` below the result's last bit
    let mut kept: u64 = if shift <= 0 {
        // `l <= p` here (`shift >= l - p`), so this is exact and fits.
        debug_assert!(!sticky, "bd_round_binary: sticky with an exact significand");
        bd_mag_low_u64(m) << (-shift) as u32
    } else if shift > l {
        // `m < 2^(shift - 1)`: below half the last place, rounds to zero.
        0
    } else {
        let sh = shift as u64; // 1..=l
        let kept = bd_mag_low_u64(&m.shr(sh as u32));
        let words = m.mag_le();
        let half = bd_mag_bit(words, sh - 1);
        let below = sticky || bd_mag_low_bits_nonzero(words, sh - 1);
        if half && (below || kept & 1 == 1) {
            kept + 1
        } else {
            kept
        }
    };
    if kept >> fmt.p != 0 {
        // Rounded up to `2^p`: one more exponent, the low bit is zero.
        kept >>= 1;
        lsb += 1;
    }
    if kept == 0 {
        return 0;
    }
    let frac_bits = fmt.p - 1;
    if kept >> frac_bits == 0 {
        // Subnormal (only when `lsb == min_lsb`): the biased exponent is 0.
        return kept;
    }
    let biased = lsb + i64::from(frac_bits) + fmt.bias;
    if biased >= fmt.inf_biased as i64 {
        return fmt.inf_biased << frac_bits;
    }
    ((biased as u64) << frac_bits) | (kept & ((1u64 << frac_bits) - 1))
}

/// `|u| * 10^-scale` as the raw bits (sign clear) of the correctly rounded
/// value in `fmt`; `u != 0`. No decimal string anywhere:
///
/// * `scale <= 0`: `|u| * 5^t * 2^t` (`t = -scale`) is an exact integer times a
///   power of two; the clamps bound `t` by ~310 and `|u|` by ~1030 bits here.
/// * `scale > 0`: `|u| / (5^s * 2^s)`. `q = floor(|u| * 2^k / 5^s)` with `k`
///   chosen so `q` has `p + 2` or `p + 3` bits (a right shift of `|u|` when `k`
///   is negative: `floor(floor(x / 2^j) / d) == floor(x / (2^j d))`), and the
///   sticky bit is "anything was lost" (the shifted-out bits or the remainder).
///   One Knuth division of a `(p + 2 + bits(5^s))`-bit dividend.
fn bd_to_binary_bits(u: &crate::bigint::BigInt, scale: i32, fmt: &BdBinFormat) -> u64 {
    let l = u.magnitude_bits();
    let s = i64::from(scale);
    // log10(|u|) lies in [(l - 1) * log10(2), l * log10(2)).
    let lo = (l as f64 - 1.0) * std::f64::consts::LOG10_2 - s as f64;
    let hi = l as f64 * std::f64::consts::LOG10_2 - s as f64;
    let frac_bits = fmt.p - 1;
    if lo > fmt.inf_log10 {
        return fmt.inf_biased << frac_bits;
    }
    if hi < fmt.zero_log10 {
        return 0;
    }
    let mag = u.abs_value();
    if s <= 0 {
        let t = scale.unsigned_abs(); // <= ~310 past the clamp
        let m = mag.mul(&bigint_pow5_memo(t));
        return bd_round_binary(&m, i64::from(t), false, fmt);
    }
    // 0 < s <= l * log10(2) + 325.5 past the clamp.
    let d = bigint_pow5_memo(scale as u32);
    let k = i64::from(fmt.p) + 2 + d.magnitude_bits() as i64 - l as i64;
    let (num, lost) = if k >= 0 {
        (mag.shl(k as u32), false)
    } else {
        let j = k.unsigned_abs(); // < l
        (mag.shr(j as u32), bd_mag_low_bits_nonzero(mag.mag_le(), j))
    };
    let (q, r) = num.divmod(&d);
    bd_round_binary(&q, -k - s, lost || !r.is_zero(), fmt)
}

/// [`bd_to_f64`]'s answer without rendering the value (R13C-4 / BD2-3).
/// First JDK 25's own fast path (`BigDecimal.doubleValue`: a magnitude below
/// `2^52` and `|scale| <= 22` is one IEEE multiply or divide of two exact
/// operands, hence correctly rounded), then [`bd_to_binary_bits`].
fn bd_to_f64_binary(u: &crate::bigint::BigInt, scale: i32) -> f64 {
    if u.is_zero() {
        // `bd(0, s)` is `0.0`, never `-0.0`, at every scale (MEASURED above).
        return 0.0;
    }
    let r = if u.magnitude_bits() <= 52 && scale.unsigned_abs() < BD_F64_POW10.len() as u32 {
        let v = bd_mag_low_u64(u) as f64; // exact: < 2^52
        if scale >= 0 {
            v / BD_F64_POW10[scale as usize]
        } else {
            v * BD_F64_POW10[scale.unsigned_abs() as usize]
        }
    } else {
        f64::from_bits(bd_to_binary_bits(u, scale, &BD_BINARY64))
    };
    // Round-to-nearest is symmetric, so the sign goes on last (`-0.0` for a
    // negative value that underflows, as the string road answers).
    if u.is_neg() {
        -r
    } else {
        r
    }
}

/// [`bd_to_f32`]'s answer without rendering the value: JDK 25's
/// `floatValue` fast path (`|intCompact| < 2^22`, `|scale| <= 10`) and then
/// one rounding of the exact value straight to `f32` (never through `f64`:
/// double rounding is observable).
fn bd_to_f32_binary(u: &crate::bigint::BigInt, scale: i32) -> f32 {
    if u.is_zero() {
        return 0.0;
    }
    let r = if u.magnitude_bits() <= 22 && scale.unsigned_abs() < BD_F32_POW10.len() as u32 {
        let v = bd_mag_low_u64(u) as f32; // exact: < 2^22
        if scale >= 0 {
            v / BD_F32_POW10[scale as usize]
        } else {
            v * BD_F32_POW10[scale.unsigned_abs() as usize]
        }
    } else {
        f32::from_bits(bd_to_binary_bits(u, scale, &BD_BINARY32) as u32)
    };
    if u.is_neg() {
        -r
    } else {
        r
    }
}

/// `doubleValue()` through [`bd_to_f64_binary`]
/// (`CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE`, default on).
fn native_bd_double_value_binary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    Ok(Some(Value::Double(bd_to_f64_binary(&u, scale))))
}

/// `floatValue()` through [`bd_to_f32_binary`] (same switch; the shipping
/// registrar also registers it since round 13 wave 13, see
/// [`bd_float_value_real_jdk_body`]).
fn native_bd_float_value_binary(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    Ok(Some(Value::Float(bd_to_f32_binary(&u, scale))))
}

/// The `doubleValue()`/`floatValue()` bodies `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE`
/// selects at registration: the binary ones by default, `=0` the rendering ones.
fn bd_double_float_value_bodies() -> (
    fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
    fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
) {
    let binary =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE");
    let double_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if binary {
        native_bd_double_value_binary
    } else {
        native_bd_double_value
    };
    let float_body: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = if binary {
        native_bd_float_value_binary
    } else {
        native_bd_float_value
    };
    (double_body, float_body)
}

/// The shipping registrar's `floatValue()` body (round 13 wave 13, BD3-1):
/// the binary one, or `None` -- the JDK bytecode answers -- when
/// `CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE=0` or the binary bodies are off.
fn bd_float_value_real_jdk_body() -> Option<fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult>
{
    let on = cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE")
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE");
    if on {
        Some(native_bd_float_value_binary)
    } else {
        None
    }
}

fn native_bd_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    Ok(Some(Value::Double(bd_to_f64(&u, scale))))
}

fn native_bd_float_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    Ok(Some(Value::Float(bd_to_f32(&u, scale))))
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

/// `native_bd_precision` counting the digits without rendering them (round
/// 13 wave 9, lane bigdec2, `CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER`). The
/// body above built the whole decimal `String` (`to_decimal`, O(words^2)) to
/// take its length; `bigint_decimal_digit_count` is a bit-length bound and at
/// most one comparison against a power of ten, with the same answer (zero is
/// 1, the sign is not a digit).
fn native_bd_precision_counted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let p = bd_precision_of(ctx, this);
    if p > 0 {
        return Ok(Some(Value::Int(p)));
    }
    let (u, _s) = bd_unscaled_bigint(ctx, this);
    let computed = bigint_decimal_digit_count(&u);
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
    // PERF (2026-08-18): the flip is `neg_value()`, a sign-bit change on the
    // limbs. It used to render the unscaled value to a decimal `String`, edit
    // its leading '-', and re-parse the result — two O(digits^2) conversions
    // for one boolean. `neg_value` already normalises zero to non-negative,
    // which is what the `dec == "0"` arm was protecting.
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    let negated = u.neg_value();
    if let Some(c) = bd_result_constant(ctx, &negated, scale) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &negated, scale);
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_abs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Limb `abs_value()`, not a decimal render + '-'-strip + re-parse — see
    // `native_bd_negate`.
    let (u, scale) = bd_unscaled_bigint(ctx, this);
    let result = bd_alloc_bigint(ctx, &u.abs_value(), scale);
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_signum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Exact sign from the unscaled value — this native shadows `signum()`
    // inside the real `compareTo` bytecode, so an f64 parse (the old
    // implementation; underflows past ~1e-324) must not decide ordering.
    //
    // PERF (2026-08-18): exactness never needed the DIGITS. This read the whole
    // magnitude and rendered it to a decimal `String` to look at its first
    // byte — 738 ns/call against 148 ns for the bignum natives that only touch
    // a field, and `BigDecimal.signum()` runs 8x per iteration of the
    // commons-math `LegendreHighPrecisionTest` inner loop (it is on the real
    // `compareTo`/`doRound` path), which made it ~12% of that benchmark on its
    // own. The sign is already stored: `intCompact` carries it for a compact
    // value, and the backing `BigInteger`'s own `signum:I` slot carries it for
    // an inflated one. Neither needs `mag[]`.
    if let Some((iv_i, _sc_i, _pr_i, ic_i)) = bd_layout(ctx) {
        if let Value::Long(ic) = ctx.get_field(this, ic_i) {
            if ic != BD_INFLATED {
                return Ok(Some(Value::Int(ic.signum() as i32)));
            }
        }
        if let Value::Object(Some(bi)) = ctx.get_field(this, iv_i) {
            if let Some((sig_i, _mag_i)) = bi_layout(ctx) {
                if let Value::Int(sg) = ctx.get_field(bi, sig_i) {
                    return Ok(Some(Value::Int(sg)));
                }
            }
        }
        // `intCompact == INFLATED` with a null/unreadable `intVal` is the
        // zero `bd_unscaled_bigint` reports for the same state.
        return Ok(Some(Value::Int(0)));
    }
    // Synthetic-stub layout: no `intCompact` slot to read.
    let (u, _scale) = bd_unscaled_bigint(ctx, this);
    Ok(Some(Value::Int(u.signum())))
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
    // MEASURED 2026-08-13 (scratchpad/orch/V3.java): an out-of-range rounding
    // mode is IllegalArgumentException "Invalid rounding mode" -- for 99, for
    // -1 and for 8 (one past the last valid mode, UNNECESSARY == 7). The call
    // used to RETURN, which is the worst shape: a caller passing a bad
    // constant got a plausible number instead of a refusal.
    if !(0..=7).contains(&mode) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Invalid rounding mode".into(),
        }
        .into());
    }
    let (unscaled, scale) = bd_unscaled_bigint(ctx, this);
    // JDK 25 `BigDecimal.setScale`, in its order:
    //
    //     if (newScale == oldScale) return this;
    //     if (this.signum() == 0)   return zeroValueOf(newScale);
    //     … int raise = checkScale((long) newScale - oldScale);   // or drop
    //
    // The scale DIFFERENCE has to be computed in a `long`, and that is the
    // whole of `checkScale`'s job. Here it used to be `new_scale - scale` and
    // `(scale - new_scale) as usize` — plain `i32` subtractions of two
    // caller-chosen scales, so `x.setScale(Integer.MIN_VALUE)` OVERFLOWED:
    // a panic in a debug build (a panic is not a Java throwable — it takes the
    // VM down, it cannot be caught) and a wrap in release, after which
    // `"0".repeat(drop)` asks for up to 2 GB of '0' from one ordinary call.
    if new_scale == scale {
        // Round 13 wave 9 (lane bigdec2): the JDK's `return this;` -- the
        // same object (`x.setScale(x.scale()) == x`), which also keeps its
        // `stringCache`. `CRATONVM_BIGNUM_JDK_IDENTITY=0` allocates a copy.
        if w9_switch(ctx, W9_JDK_IDENTITY) {
            return Ok(Some(Value::Object(Some(this))));
        }
        let same = bd_alloc_bigint(ctx, &unscaled, new_scale);
        return Ok(Some(Value::Object(Some(same?))));
    }
    if unscaled.is_zero() {
        // `zeroValueOf(newScale)`: a zero unscaled value takes ANY scale and
        // `checkScale` is never consulted. MEASURED (`scratchpad/f7/Scale.java`,
        // Microsoft OpenJDK 25.0.3+9): `ZERO.setScale(Integer.MAX_VALUE)` is
        // `0E-2147483647` and `ZERO.setScale(Integer.MIN_VALUE)` is
        // `0E+2147483648` — neither throws.
        if let Some(c) = bd_result_constant(ctx, &unscaled, new_scale) {
            return Ok(Some(Value::Object(Some(c))));
        }
        let zero = bd_alloc_bigint(ctx, &unscaled, new_scale);
        return Ok(Some(Value::Object(Some(zero?))));
    }
    let diff = i64::from(new_scale) - i64::from(scale);
    if diff.abs() > i64::from(i32::MAX) {
        // `BigDecimal.checkScale`:
        //     asInt = val > Integer.MAX_VALUE ? Integer.MAX_VALUE : Integer.MIN_VALUE;
        //     if (…nonzero…) throw new ArithmeticException(asInt > 0 ? "Underflow" : "Overflow");
        // Both call sites pass the POSITIVE magnitude (`newScale - oldScale`
        // when raising, `oldScale - newScale` when dropping), so an
        // out-of-range difference always clamps to `Integer.MAX_VALUE` and the
        // message is always "Underflow". MEASURED:
        // `new BigDecimal("1.5").setScale(Integer.MIN_VALUE, HALF_UP)`
        // !! ArithmeticException: Underflow   [0 ms].
        return Err(bd_underflow());
    }
    if diff > 0 {
        let raise = diff as i32;
        bd_pow_ten_check(raise)?;
        let padded = bigint_mul_pow10(&unscaled, raise);
        if let Some(c) = bd_result_constant(ctx, &padded, new_scale) {
            return Ok(Some(Value::Object(Some(c))));
        }
        let result = bd_alloc_bigint(ctx, &padded, new_scale);
        return Ok(Some(Value::Object(Some(result?))));
    }
    // 0 < -diff <= i32::MAX, so both narrowings below are exact.
    let drop_scale = (-diff) as i32;
    bd_pow_ten_check(drop_scale)?;
    // `bigint_pow10`, not `"1" + "0"*drop` through `BigInt::from_decimal`:
    // `bd_pow_ten_check` ADMITS `drop` up to 715_827_882 (HotSpot really tries
    // there — see `bigint_mul_pow10`), so this was a 715 MB `String` followed
    // by an O(n²) digit-at-a-time parse for a divisor the square-and-multiply
    // builds directly. Same value, and it was the fourth copy of the spelling.
    let divisor = bigint_pow10(drop_scale as u32);
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
    // `divideAndRound` / `MutableBigInteger.toBigDecimal` end in
    // `valueOf(q, scale)` / `zeroValueOf(scale)` for every small quotient.
    if let Some(c) = bd_result_constant(ctx, &rounded, new_scale) {
        return Ok(Some(Value::Object(Some(c))));
    }
    let result = bd_alloc_bigint(ctx, &rounded, new_scale);
    Ok(Some(Value::Object(Some(result?))))
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
    let a = bd_read_unchecked(ctx, this);
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
    Ok(Some(Value::Object(Some(result?))))
}

fn native_bd_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = bd_read_unchecked(ctx, this);
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
        assert_eq!(s(&format!("-{big}"), 16), "-18ee90ff6c373e0ee4e3f0ad2");
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

/// The synthetic-layout `BigDecimal` string parser, and the two defects it was
/// written to close.
#[cfg(test)]
mod synthetic_bigdecimal_exact_tests {
    use super::{bd_compare_unscaled, bd_exact_divide_core, bd_parse_decimal_str};
    use crate::bigint::BigInt;
    use std::cmp::Ordering;

    fn parse(s: &str) -> (String, i32) {
        let (u, sc) = bd_parse_decimal_str(s).unwrap_or_else(|| panic!("{s:?} must parse"));
        (u.to_decimal(), sc)
    }

    /// Every expected pair is what `new java.math.BigDecimal(s)` reports as
    /// `(unscaledValue(), scale())` on a JDK -- the contract, not a guess.
    #[test]
    fn it_builds_the_pair_the_jdk_constructor_builds() {
        assert_eq!(parse("1.25"), ("125".into(), 2));
        assert_eq!(parse("-1.25"), ("-125".into(), 2));
        assert_eq!(parse("+7"), ("7".into(), 0));
        assert_eq!(parse("100"), ("100".into(), 0));
        assert_eq!(parse(".5"), ("5".into(), 1));
        assert_eq!(parse("5."), ("5".into(), 0));
        assert_eq!(parse("0.00"), ("0".into(), 2));
        // The exponent adjusts the scale, and this is the case the old synthetic
        // arm got wrong: it turned "1.5E+10" into the integer 1510.
        assert_eq!(parse("1.5E+10"), ("15".into(), -9));
        assert_eq!(parse("1.5e10"), ("15".into(), -9));
        assert_eq!(parse("2.5E-3"), ("25".into(), 4));
        assert_eq!(parse("  42  "), ("42".into(), 0), "surrounding whitespace");
    }

    /// Outside the grammar is `None`, never a number assembled from whatever
    /// digits happened to be present -- which is what `BigInt::from_decimal`
    /// alone would do, since it skips non-digits.
    #[test]
    fn it_refuses_what_is_not_a_decimal() {
        for bad in [
            "", ".", "-", "e5", "1e", "1e+", "1.2.3", "12a", "1,5", "NaN", "Infinity", "--1",
        ] {
            assert!(
                bd_parse_decimal_str(bad).is_none(),
                "{bad:?} must be refused"
            );
        }
    }

    /// A scale outside `i32` is refused rather than wrapped: `BigDecimal`'s scale
    /// is an `int`, so such a string names no `BigDecimal`.
    #[test]
    fn it_refuses_a_scale_that_does_not_fit_an_int() {
        assert!(bd_parse_decimal_str("1E-3000000000").is_none());
        assert!(bd_parse_decimal_str("1E+3000000000").is_none());
    }

    /// **Defect 1, closed.** Two values agreeing in their leading ~17 digits
    /// parse to the same `f64`, so the old synthetic `compareTo` called them
    /// EQUAL. Through the exact parse they order correctly, and the `f64`
    /// comparison is asserted alongside so this test proves the difference and
    /// not merely an outcome.
    #[test]
    fn near_equal_values_are_ordered_exactly() {
        let a = "0.12345678901234567890";
        let b = "0.12345678901234567891";
        let fa: f64 = a.parse().unwrap();
        let fb: f64 = b.parse().unwrap();
        assert_eq!(fa, fb, "precondition: f64 cannot tell these apart");

        let (ua, sa) = bd_parse_decimal_str(a).unwrap();
        let (ub, sb) = bd_parse_decimal_str(b).unwrap();
        assert_eq!(bd_compare_unscaled(&ua, sa, &ub, sb), Ordering::Less);
        assert_eq!(bd_compare_unscaled(&ub, sb, &ua, sa), Ordering::Greater);
        // `compareTo` ignores scale: 2.0 and 2.00 are equal.
        let (u1, s1) = bd_parse_decimal_str("2.0").unwrap();
        let (u2, s2) = bd_parse_decimal_str("2.00").unwrap();
        assert_eq!(bd_compare_unscaled(&u1, s1, &u2, s2), Ordering::Equal);
    }

    /// **Defect 2, closed.** The old synthetic `divide` computed `1/3` in `f64`
    /// and RETURNED `0.3333333333333333`; `BigDecimal.divide(BigDecimal)` must
    /// throw for a non-terminating expansion. Through the exact core it does,
    /// and a terminating division still returns the exact quotient.
    #[test]
    fn divide_throws_for_a_non_terminating_expansion_and_is_exact_otherwise() {
        let one = bd_parse_decimal_str("1").unwrap();
        let three = bd_parse_decimal_str("3").unwrap();
        assert!(
            bd_exact_divide_core(&one.0, one.1, &three.0, three.1).is_err(),
            "1/3 does not terminate and must raise, not round"
        );
        let four = bd_parse_decimal_str("4").unwrap();
        let (q, scale) =
            bd_exact_divide_core(&one.0, one.1, &four.0, four.1).expect("1/4 terminates");
        assert_eq!((q.to_decimal(), scale), ("25".into(), 2), "1/4 = 0.25");
        let zero = (BigInt::zero(), 0);
        assert!(
            bd_exact_divide_core(&one.0, one.1, &zero.0, zero.1).is_err(),
            "division by zero raises"
        );
    }
}

/// Round 13 wave 8 (lane bigdec): `pow(int, MathContext)`, the one-division
/// `divide(BigDecimal, MathContext)`, and the factored `BigInteger.pow`.
/// Expected rows are worked through the JDK 25 source of the method named
/// (`BigDecimal.java` / `BigInteger.java`), step by step, in each comment.
#[cfg(test)]
mod r13w8_bigdec_tests {
    use super::{
        bd_divide_round_core, bd_divide_round_core_oneshot, bd_pow_exact_core, bd_pow_mc_core,
        bd_round_to_precision, bigint_pow_checked, bigint_pow_limbs,
        register_bigdecimal_arithmetic_overrides,
    };
    use crate::bigint::BigInt;
    use crate::{
        BD_ROUND_CEILING, BD_ROUND_DOWN, BD_ROUND_FLOOR, BD_ROUND_HALF_DOWN, BD_ROUND_HALF_EVEN,
        BD_ROUND_HALF_UP, BD_ROUND_UNNECESSARY, BD_ROUND_UP,
    };
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::error::{MethodCallFailed, VmError};

    fn thrown(e: MethodCallFailed) -> String {
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(r)) => format!("{r}"),
            other => format!("{other}"),
        }
    }

    fn big(v: i64) -> BigInt {
        BigInt::from_decimal(&v.to_string())
    }

    fn div(a: i64, sa: i32, b: i64, sb: i32, p: i32, mode: i32) -> Result<(String, i32), String> {
        bd_divide_round_core_oneshot(&big(a), sa, &big(b), sb, p, mode)
            .map(|(q, s)| (q.to_decimal(), s))
            .map_err(thrown)
    }

    fn pow(u: &str, s: i32, n: i32, p: i32, mode: i32) -> Result<(String, i32), String> {
        bd_pow_mc_core(&BigInt::from_decimal(u), s, n, p, mode)
            .map(|(q, sc)| (q.to_decimal(), sc))
            .map_err(thrown)
    }

    fn ok(u: &str, s: i32) -> Result<(String, i32), String> {
        Ok((u.to_string(), s))
    }

    /// The defect: an integer quotient already longer than the precision.
    /// `12345/7 @2`: xprec 5, yprec 1, 12345 < 7e4 so no adjust; scl = 1-5+2
    /// = -2; raise = -2, so newScale = 3 and the divisor is 7*10^2: 12345/700
    /// = 17 r 445, 890 > 700 -> 18E+2 = 1.8E+3. The digit loop said 1.76E+3.
    #[test]
    fn divide_mc_rounds_an_over_long_integer_quotient() {
        assert_eq!(div(12345, 0, 7, 0, 2, BD_ROUND_HALF_UP), ok("18", -2));
        // DECIMAL32 of 10^8/3: 3.333333E+7.
        assert_eq!(
            div(100_000_000, 0, 3, 0, 7, BD_ROUND_HALF_UP),
            ok("3333333", -1)
        );
        // 999 @2: 99 r 9 of 999/10, rounds to 100, doRound drops the carry
        // digit: 1.0E+3.
        assert_eq!(div(999, 0, 1, 0, 2, BD_ROUND_HALF_UP), ok("10", -2));
        let old = bd_divide_round_core(&big(12345), 0, &big(7), 0, 2, BD_ROUND_HALF_UP)
            .map(|(q, s)| (q.to_decimal(), s))
            .map_err(thrown);
        assert_ne!(
            old,
            ok("18", -2),
            "the replaced loop is the defect this pins"
        );
    }

    #[test]
    fn divide_mc_matches_the_jdk_on_worked_rows() {
        assert_eq!(div(1, 0, 3, 0, 10, BD_ROUND_HALF_UP), ok("3333333333", 10));
        // Exact: 100000/4 at scl 5, stripped to the preferred scale 0 as far
        // as the zeros go: 0.25.
        assert_eq!(div(1, 0, 4, 0, 5, BD_ROUND_HALF_UP), ok("25", 2));
        // 100/4 @1: 100/40 = 2 r 20, exactly half.
        assert_eq!(div(100, 0, 4, 0, 1, BD_ROUND_HALF_UP), ok("3", -1));
        assert_eq!(div(100, 0, 4, 0, 1, BD_ROUND_HALF_EVEN), ok("2", -1));
        // -1/8 @2 FLOOR: -100/8 = -12 r -4, away from zero: -0.13.
        assert_eq!(div(-1, 0, 8, 0, 2, BD_ROUND_FLOOR), ok("-13", 2));
        // 19/20 @1: 190/20 = 9 r 10 (half), up to 10, doRound: 1.
        assert_eq!(div(19, 0, 20, 0, 1, BD_ROUND_HALF_UP), ok("1", 0));
        // x' == y': the quotient is exactly 10^mcp and reduces to 1.
        assert_eq!(div(5, 0, 5, 0, 3, BD_ROUND_HALF_UP), ok("1", 0));
        assert_eq!(
            div(1, 0, 3, 0, 5, BD_ROUND_UNNECESSARY),
            Err("ArithmeticException: Rounding necessary".to_string())
        );
        assert_eq!(
            div(1, 0, 0, 0, 5, BD_ROUND_HALF_UP),
            Err("ArithmeticException: Division by zero".to_string())
        );
        assert_eq!(div(0, 3, 7, 1, 5, BD_ROUND_HALF_UP), ok("0", 2));
    }

    /// Differential against the replaced digit loop over every shape it got
    /// right (all but "integer quotient longer than `p` with a remainder").
    #[test]
    fn divide_mc_agrees_with_the_digit_loop_where_the_loop_was_right() {
        let modes = [
            BD_ROUND_UP,
            BD_ROUND_DOWN,
            BD_ROUND_CEILING,
            BD_ROUND_FLOOR,
            BD_ROUND_HALF_UP,
            BD_ROUND_HALF_DOWN,
            BD_ROUND_HALF_EVEN,
            BD_ROUND_UNNECESSARY,
        ];
        let mut checked = 0usize;
        for a in (-250i64..=250).step_by(13) {
            for b in [-37i64, -8, -3, -1, 1, 2, 3, 7, 8, 12, 40, 125] {
                let (ua, ub) = (big(a), big(b));
                let (q0, r0) = ua.divmod(&ub);
                let q0_digits = if q0.is_zero() {
                    0
                } else {
                    q0.abs_value().to_decimal().len() as i32
                };
                for (sa, sb) in [(0, 0), (2, 0), (0, 3), (-1, 1)] {
                    for p in 1..=5 {
                        if !r0.is_zero() && q0_digits > p {
                            continue;
                        }
                        for &m in &modes {
                            let old = bd_divide_round_core(&ua, sa, &ub, sb, p, m)
                                .map(|(q, s)| (q.to_decimal(), s))
                                .map_err(thrown);
                            let new = bd_divide_round_core_oneshot(&ua, sa, &ub, sb, p, m)
                                .map(|(q, s)| (q.to_decimal(), s))
                                .map_err(thrown);
                            assert_eq!(new, old, "{a}e-{sa} / {b}e-{sb} @{p} mode {m}");
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 3_000, "vacuous differential: {checked}");
    }

    #[test]
    fn pow_mc_follows_x3274() {
        // 2^3 @1: acc 2, squared 4, times 2 = 8.
        assert_eq!(pow("2", 0, 3, 1, BD_ROUND_HALF_UP), ok("8", 0));
        // 4^-2 @5: work precision 7, acc 16, ONE.divide(16) exact at scl 8,
        // stripped to 0.0625.
        assert_eq!(pow("4", 0, -2, 5, BD_ROUND_HALF_UP), ok("625", 4));
        assert_eq!(pow("2", 0, -1, 3, BD_ROUND_HALF_UP), ok("5", 1));
        assert_eq!(pow("7", 3, 0, 5, BD_ROUND_HALF_UP), ok("1", 0));
        let invalid = Err("ArithmeticException: Invalid operation".to_string());
        // mc.precision == 0 is pow(n), which refuses n < 0.
        assert_eq!(pow("2", 0, -1, 0, BD_ROUND_HALF_UP), invalid);
        // elength(100) = 3 > precision 2.
        assert_eq!(pow("2", 0, 100, 2, BD_ROUND_HALF_UP), invalid);
        assert_eq!(pow("2", 0, 1_000_000_000, 20, BD_ROUND_HALF_UP), invalid);
        assert_eq!(
            pow("0", 0, -3, 5, BD_ROUND_HALF_UP),
            Err("ArithmeticException: Division by zero".to_string())
        );
    }

    /// Where no working-precision rounding happens the X3.274 result IS the
    /// exact power rounded once, which is what the replaced body computed.
    #[test]
    fn pow_mc_equals_round_once_when_nothing_is_rounded_on_the_way() {
        for base in 1..=40i64 {
            for scale in [0, 1, 3, -2] {
                for n in 1..=7 {
                    let u = big(base);
                    let mut exact = big(1);
                    for _ in 0..n {
                        exact = exact.mul(&u);
                    }
                    let (want_u, want_s) =
                        bd_round_to_precision(&exact, scale * n, 30, BD_ROUND_HALF_EVEN)
                            .expect("rounding at 30 digits");
                    let got = bd_pow_mc_core(&u, scale, n, 30, BD_ROUND_HALF_EVEN)
                        .map(|(q, s)| (q.to_decimal(), s))
                        .map_err(thrown);
                    assert_eq!(
                        got,
                        Ok((want_u.to_decimal(), want_s)),
                        "{base}e-{scale}^{n}"
                    );
                }
            }
        }
    }

    /// The shape that was O(n^2): 1.0001^100000 @ DECIMAL64. ~34 rounded
    /// multiplies of <= 22-digit numbers; e^9.9995 = 22015.4..., 16 digits.
    #[test]
    fn pow_mc_large_exponent_stays_at_working_precision() {
        let (q, s) =
            bd_pow_mc_core(&big(10001), 4, 100_000, 16, BD_ROUND_HALF_EVEN).expect("in range");
        assert_eq!(s, 11);
        let digits = q.to_decimal();
        assert_eq!(digits.len(), 16);
        assert!(digits.starts_with("22015"), "{digits}");
    }

    #[test]
    fn pow_exact_scale_refusals_follow_check_scale() {
        let zero = BigInt::zero();
        let (q, s) = bd_pow_exact_core(&zero, i32::MAX, 2).expect("zero clamps");
        assert!(q.is_zero());
        assert_eq!(s, i32::MAX);
        assert_eq!(
            bd_pow_exact_core(&big(1), i32::MAX, 2)
                .map_err(thrown)
                .err(),
            Some("ArithmeticException: Underflow".to_string())
        );
        assert_eq!(
            bd_pow_exact_core(&big(1), i32::MIN, 2)
                .map_err(thrown)
                .err(),
            Some("ArithmeticException: Overflow".to_string())
        );
        let (q, s) = bd_pow_exact_core(&big(5), 1, 3).expect("small");
        assert_eq!((q.to_decimal(), s), ("125".to_string(), 3));
        let (q, s) = bd_pow_exact_core(&zero, 7, 0).expect("0^0");
        assert_eq!((q.to_decimal(), s), ("1".to_string(), 0));
    }

    /// Factoring the base's trailing zero bits out (the JDK's own shortcut)
    /// changes the route, not the value, including the sign of an odd power
    /// of a negative even base.
    #[test]
    fn factored_pow_matches_repeated_multiplication() {
        for base in [-12i64, 10, -2, 3, 1024, -6, 7, -1, 96] {
            let b = big(base);
            let mut naive = big(1);
            for e in 1..=40u32 {
                naive = naive.mul(&b);
                assert_eq!(bigint_pow_limbs(&b, e), naive, "{base}^{e}");
                let plain = cratonvm_types::flags::with_thread_overrides(
                    &[("CRATONVM_BIGINT_FAST_MUL", Some("0"))],
                    || bigint_pow_limbs(&b, e),
                );
                assert_eq!(plain, naive, "{base}^{e} unfactored");
                assert_eq!(bigint_pow_checked(&b, e as i32).ok(), Some(naive.clone()));
            }
        }
        assert_eq!(bigint_pow_checked(&BigInt::zero(), 0).ok(), Some(big(1)));
    }

    /// Each of the three switches registers its own body; `0` registers the
    /// replaced one. (Function addresses of two different bodies.)
    #[test]
    fn bigdecimal_switches_choose_the_registered_body() {
        let bd = "java/math/BigDecimal";
        let rows: [(&str, &str, &str, usize, usize); 3] = [
            (
                "CRATONVM_BIGDECIMAL_POW_MC_JDK",
                "pow",
                "(ILjava/math/MathContext;)Ljava/math/BigDecimal;",
                super::native_bd_pow_mc_jdk as usize,
                super::native_bd_pow_mc as usize,
            ),
            (
                "CRATONVM_BIGDECIMAL_DIVIDE_MC_ONESHOT",
                "divide",
                "(Ljava/math/BigDecimal;Ljava/math/MathContext;)Ljava/math/BigDecimal;",
                super::native_bd_divide_mc_oneshot as usize,
                super::native_bd_divide_mc as usize,
            ),
            (
                "CRATONVM_BIGDECIMAL_TOSTRING_CACHE",
                "toString",
                "()Ljava/lang/String;",
                super::native_bd_to_string_cached as usize,
                super::native_bd_to_string as usize,
            ),
        ];
        for (switch, name, desc, on, off) in rows {
            let mut reg = NativeMethodRegistry::new();
            register_bigdecimal_arithmetic_overrides(&mut reg);
            let (cb, _) = reg.find_with_kind(bd, name, desc).expect("registered");
            assert_eq!(cb as usize, on, "{switch} default");
            let off_reg =
                cratonvm_types::flags::with_thread_overrides(&[(switch, Some("0"))], || {
                    let mut reg = NativeMethodRegistry::new();
                    register_bigdecimal_arithmetic_overrides(&mut reg);
                    reg
                });
            let (cb, _) = off_reg.find_with_kind(bd, name, desc).expect("registered");
            assert_eq!(cb as usize, off, "{switch}=0");
        }
    }

    /// `CRATONVM_BIGNUM_STRINGS_UNINTERNED` covers `BigInteger.toString()` and
    /// `BigDecimal.toPlainString()` in the shipping registrars.
    #[test]
    fn uninterned_string_switch_chooses_the_registered_body() {
        let build = || {
            let mut reg = NativeMethodRegistry::new();
            super::register_biginteger_arithmetic_overrides(&mut reg);
            register_bigdecimal_arithmetic_overrides(&mut reg);
            reg
        };
        let bi = ("java/math/BigInteger", "toString", "()Ljava/lang/String;");
        let bd = (
            "java/math/BigDecimal",
            "toPlainString",
            "()Ljava/lang/String;",
        );
        let on = build();
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_BIGNUM_STRINGS_UNINTERNED", Some("0"))],
            build,
        );
        let cb = |reg: &NativeMethodRegistry, t: (&str, &str, &str)| {
            reg.find_with_kind(t.0, t.1, t.2).expect("registered").0 as usize
        };
        assert_eq!(cb(&on, bi), super::native_bi_to_string_uninterned as usize);
        assert_eq!(cb(&off, bi), super::native_bi_to_string as usize);
        assert_eq!(
            cb(&on, bd),
            super::native_bd_to_plain_string_uninterned as usize
        );
        assert_eq!(cb(&off, bd), super::native_bd_to_plain_string as usize);
    }

    /// The rows the orchestrator's wave-8 diff (`R13BigdecDivScaleDiff`) caught
    /// the BUILD answering wrong -- every one a rounding-mode row: the core
    /// was right and `bd_mc_precision_and_mode` read every `RoundingMode` as
    /// HALF_UP. Kept as worked rows of the core.
    #[test]
    fn divide_mc_applies_each_rounding_mode() {
        // 8403284.5, HALF_EVEN keeps the even digit.
        assert_eq!(
            div(100_839_414, 0, 12, 0, 7, BD_ROUND_HALF_EVEN),
            ok("8403284", 0)
        );
        assert_eq!(
            div(102_787_488, 0, 64, 0, 7, BD_ROUND_HALF_EVEN),
            ok("1606054", 0)
        );
        // -1.428..E+10 @1 UP is away from zero.
        assert_eq!(div(-1_000_000_000, -2, 7, 0, 1, BD_ROUND_UP), ok("-2", -10));
        assert_eq!(
            div(308_871_508, 0, 31_011, 2, 3, BD_ROUND_CEILING),
            ok("997", -3)
        );
        assert_eq!(
            div(-418_949_736, -2, 33_544, 2, 8, BD_ROUND_UP),
            ok("-12489559", -1)
        );
    }

    /// The one-division core against HotSpot 25, row for row, with the inputs
    /// rebuilt exactly as the probe builds them (Java `long` arithmetic on
    /// non-negative `i`; `MODES[i % 7]` is `BD_ROUND_*` value `i % 7`).
    #[test]
    fn divide_mc_matches_hotspot_transcript() {
        for (i, &(u, s)) in HOTSPOT_DIV_LONG.iter().enumerate() {
            let i = i as i64;
            let got = div(
                100_000_000 + 7919 * i,
                0,
                3 + i % 97,
                0,
                7,
                BD_ROUND_HALF_EVEN,
            );
            assert_eq!(got, ok(u, s), "long {i}");
        }
        for (i, &(u, s)) in HOTSPOT_DIV_MIXED.iter().enumerate() {
            let i = i as i64;
            let a = (i * 2_654_435_761) % 2_000_000_007 - 1_000_000_000;
            let mut b = if i % 13 == 0 {
                7
            } else {
                (i * 40_503) % 99_991 - 49_995
            };
            if b == 0 {
                b = 17;
            }
            let (sa, sb) = ((i % 7 - 2) as i32, (i % 5) as i32);
            let got = div(a, sa, b, sb, (1 + i % 25) as i32, (i % 7) as i32);
            assert_eq!(got, ok(u, s), "mixed {i}");
        }
    }

    /// HotSpot 25 `divide(BigDecimal, MathContext)` results for the first 150
    /// iterations of each loop of `R13BigdecDivScaleDiff` / `R13BigdecMathContext`
    /// (`jitr13-probes/ref13/R13BigdecDivScaleDiff.txt`), as `(unscaledValue, scale)`.
    /// `div-mc-long`: `(100000000 + 7919 i) / (3 + i % 97)` at `DECIMAL32` (HALF_EVEN).
    const HOTSPOT_DIV_LONG: &[(&str, i32)] = &[
        ("3333333", -1),
        ("2500198", -1),
        ("2000317", -1),
        ("1667063", -1),
        ("1429024", -1),
        ("1250495", -1),
        ("1111639", -1),
        ("1000554", -1),
        ("9096668", 0),
        ("8339273", 0),
        ("7698399", 0),
        ("7149079", 0),
        ("6673002", 0),
        ("6256434", 0),
        ("5888874", 0),
        ("5562155", 0),
        ("5269827", 0),
        ("5006731", 0),
        ("4768692", 0),
        ("4552294", 0),
        ("4354712", 0),
        ("4173596", 0),
        ("4006969", 0),
        ("3853159", 0),
        ("3710743", 0),
        ("3578499", 0),
        ("3455376", 0),
        ("3340460", 0),
        ("3232959", 0),
        ("3132177", 0),
        ("3037502", 0),
        ("2948397", 0),
        ("2864383", 0),
        ("2785037", 0),
        ("2709980", 0),
        ("2638873", 0),
        ("2571412", 0),
        ("2507325", 0),
        ("2446364", 0),
        ("2388306", 0),
        ("2332948", 0),
        ("2280106", 0),
        ("2229613", 0),
        ("2181316", 0),
        ("2135073", 0),
        ("2090757", 0),
        ("2048250", 0),
        ("2007444", 0),
        ("1968237", 0),
        ("1930539", 0),
        ("1894263", 0),
        ("1859331", 0),
        ("1825669", 0),
        ("1793209", 0),
        ("1761888", 0),
        ("1731647", 0),
        ("1702432", 0),
        ("1674190", 0),
        ("1646874", 0),
        ("1620439", 0),
        ("1594843", 0),
        ("1570048", 0),
        ("1546015", 0),
        ("1522711", 0),
        ("1500102", 0),
        ("1478158", 0),
        ("1456850", 0),
        ("1436151", 0),
        ("1416035", 0),
        ("1396478", 0),
        ("1377457", 0),
        ("1358949", 0),
        ("1340936", 0),
        ("1323396", 0),
        ("1306312", 0),
        ("1289666", 0),
        ("1273441", 0),
        ("1257622", 0),
        ("1242194", 0),
        ("1227141", 0),
        ("1212452", 0),
        ("1198112", 0),
        ("1184110", 0),
        ("1170433", 0),
        ("1157071", 0),
        ("1144013", 0),
        ("1131248", 0),
        ("1118766", 0),
        ("1106559", 0),
        ("1094617", 0),
        ("1082932", 0),
        ("1071496", 0),
        ("1060301", 0),
        ("1049338", 0),
        ("1038602", 0),
        ("1028085", 0),
        ("1017780", 0),
        ("3358938", -1),
        ("2519402", -1),
        ("2015680", -1),
        ("1679865", -1),
        ("1439997", -1),
        ("1260097", -1),
        ("1120174", -1),
        ("1008236", -1),
        ("9166500", 0),
        ("8403284", 0),
        ("7757487", 0),
        ("7203947", 0),
        ("6724211", 0),
        ("6304443", 0),
        ("5934059", 0),
        ("5604829", 0),
        ("5310255", 0),
        ("5045138", 0),
        ("4805271", 0),
        ("4587209", 0),
        ("4388110", 0),
        ("4205602", 0),
        ("4037694", 0),
        ("3882703", 0),
        ("3739193", 0),
        ("3605933", 0),
        ("3481863", 0),
        ("3366065", 0),
        ("3257738", 0),
        ("3156181", 0),
        ("3060779", 0),
        ("2970989", 0),
        ("2886330", 0),
        ("2806374", 0),
        ("2730740", 0),
        ("2659087", 0),
        ("2591108", 0),
        ("2526529", 0),
        ("2465099", 0),
        ("2406595", 0),
        ("2350812", 0),
        ("2297564", 0),
        ("2246683", 0),
        ("2198014", 0),
        ("2151417", 0),
        ("2106760", 0),
        ("2063927", 0),
        ("2022807", 0),
        ("1983299", 0),
        ("1945311", 0),
        ("1908756", 0),
        ("1873556", 0),
        ("1839635", 0),
    ];

    /// `div-mc-mixed`: `valueOf(a, i % 7 - 2) / valueOf(b, i % 5)` at
    /// `MathContext(1 + i % 25, MODES[i % 7])`; see `divide_mc_matches_hotspot_transcript`.
    const HOTSPOT_DIV_MIXED: &[(&str, i32)] = &[
        ("-2", -10),
        ("36", -5),
        ("997", -3),
        ("-3383", -3),
        ("-31786", -2),
        ("-573467", 5),
        ("-1331534", 4),
        ("-12489559", -1),
        ("-907670436", 1),
        ("6112519899", 1),
        ("10141389325", 7),
        ("-449148831902", 8),
        ("2365022111040", 9),
        ("-70333603714286", 7),
        ("9484", -6),
        ("-1925974813661666", 10),
        ("27946538351822504", 10),
        ("324806482776482776", 13),
        ("-3735240621707060063", 13),
        ("-28826533299363057325", 14),
        ("-222549828161452976444", 21),
        ("11611731125", 1),
        ("-14641943003670126145395", 15),
        ("-283577503406922867266285", 17),
        ("3188275327195595270331257", 17),
        ("2", -2),
        ("22", -3),
        ("153", 0),
        ("4274", -6),
        ("-86522", -3),
        ("-181933", 1),
        ("-1248672", 1),
        ("-12563774", 4),
        ("-449115003", 4),
        ("-2751926392", 5),
        ("29366689500", 5),
        ("679314318485", 5),
        ("-1612320352057", 6),
        ("12231661269309", 7),
        ("747134745714286", 5),
        ("2766707917661700", 14),
        ("-15608445692536205", 15),
        ("-998237903358239593", 10),
        ("1046222666504322415", 9),
        ("-63433546608857231340", 12),
        ("-165309519817633649534", 17),
        ("-6733975355129650507328", 18),
        ("52296227399692527553429", 20),
        ("-726964941901408450704225", 20),
        ("-2678253355540878155242225", 14),
        ("1", -5),
        ("24", -4),
        ("-139", -7),
        ("1001", -3),
        ("90891", 0),
        ("-449108", 5),
        ("-1913875", -1),
        ("-73652711", 1),
        ("-155907753", -1),
        ("-9734278799", 3),
        ("-13579524823", 8),
        ("440385027268", 9),
        ("1101449803027", 10),
        ("11956871391350", 3),
        ("208356069020791", 5),
        ("-6595373385714285", 8),
        ("82245857831633741", 13),
        ("-234999132062910876", 13),
        ("-1119426958670260557", 13),
        ("34716243354465576688", 16),
        ("-557698084359733021400", 14),
        ("-2060306611474778590681", 15),
        ("-35614931380153947132883", 17),
        ("110781657981388690050107", 16),
        ("-1203997582547169811320755", 18),
        ("-7", 0),
        ("26", 0),
        ("197", -6),
        ("6569", -7),
        ("-14016", -4),
        ("681461", 2),
        ("2995197", 5),
        ("-23328119", 4),
        ("-565789235", 5),
        ("5775418556", 1),
        ("-90429607874", 5),
        ("-214044312311", 6),
        ("2472687912342", 8),
        ("40479028798683", 8),
        ("168185038526635", 9),
        ("2289987343785503", 15),
        ("79093344428571429", 6),
        ("338626033524330796", 10),
        ("-8031494303908395162", 12),
        ("-12200813433716457011", 12),
        ("443577937366167023555", 17),
        ("-4508032431732884690048", 20),
        ("-23031118831822759315206", 20),
        ("-440354367430025445292621", 14),
        ("5292950901149656107234298", 16),
        ("7", -5),
        ("-22", -3),
        ("135", -2),
        ("1834", -1),
        ("-13410", -4),
        ("-888825", -1),
        ("8470966", 1),
        ("61783402", 1),
        ("-129850629", 2),
        ("-9590962297", 4),
        ("17238399843", 8),
        ("-773525020548", 11),
        ("-2239355636035", 4),
        ("34909169143821", 5),
        ("122312820807097", 6),
        ("3147524818489835", 12),
        ("18753403018434597", 13),
        ("-61573864", 1),
        ("7501803304009133033", 16),
        ("-29551419241903992460", 9),
        ("-433184952301565249606", 15),
        ("-3834917356390298001684", 17),
        ("-10273103505129457743039", 16),
        ("-156089289184589200061891", 18),
        ("-5521407868104368306775108", 20),
        ("6", 0),
        ("12", -6),
        ("-200", -5),
        ("2204", -4),
        ("23449", -3),
        ("109497", 0),
        ("-1675836", 4),
        ("19682280", 4),
        ("106885801", 0),
        ("-3141044623", 0),
        ("-35397560230", 6),
        ("-793885914534", 10),
        ("-1116634957555", 6),
        ("-17233709049456", 9),
        ("170790380096046", 11),
        ("2967199794543457", 9),
        ("18790918021732928", 10),
        ("-360716327160493827", 11),
        ("8347321428571428571", 9),
        ("44671777067073528549", 13),
        ("-455272351035717330151", 20),
        ("-1520068159107311386221", 20),
        ("-17823196292159928523565", 13),
        ("-319050120053357047576700", 16),
        ("-3521691549489936586710780", 16),
    ];
}

/// Round 13 wave 9 (lane bigdec2): the residuals of
/// `r13w8-bigdec-bignum-residuals-FIXED-20260929.md` (docs/known-issues/jit).
#[cfg(test)]
mod r13w9_bigdec2_tests {
    use super::{
        bd_add_mc_core, bd_exact_divide_core, bd_exact_divide_core_factored, bd_rescale_operand,
        bd_round_to_precision, bigint_pow_limbs, native_bd_add_mc, native_bd_compare_to_exact,
        native_bd_divide_exact, register_bigdecimal_arithmetic_overrides,
        register_biginteger_arithmetic_overrides,
    };
    use crate::bigint::BigInt;
    use crate::test_utils::mock_ctx;
    use crate::{
        BD_ROUND_CEILING, BD_ROUND_DOWN, BD_ROUND_FLOOR, BD_ROUND_HALF_DOWN, BD_ROUND_HALF_EVEN,
        BD_ROUND_HALF_UP, BD_ROUND_UNNECESSARY, BD_ROUND_UP,
    };
    use cratonvm_native_api::{NativeHeapAccess, NativeMethodRegistry};
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
    use cratonvm_types::Value;

    const MODES: [i32; 8] = [
        BD_ROUND_UP,
        BD_ROUND_DOWN,
        BD_ROUND_CEILING,
        BD_ROUND_FLOOR,
        BD_ROUND_HALF_UP,
        BD_ROUND_HALF_DOWN,
        BD_ROUND_HALF_EVEN,
        BD_ROUND_UNNECESSARY,
    ];

    fn thrown(e: MethodCallFailed) -> String {
        match e {
            MethodCallFailed::InternalError(VmError::Runtime(r)) => format!("{r}"),
            other => format!("{other}"),
        }
    }

    fn big(v: i64) -> BigInt {
        BigInt::from_decimal(&v.to_string())
    }

    fn shown(r: Result<(BigInt, i32), MethodCallFailed>) -> Result<(String, i32), String> {
        r.map(|(q, s)| (q.to_decimal(), s)).map_err(thrown)
    }

    /// Item 1: the factored exact divide is the digit loop's answer on every
    /// shape, the scale-extreme refusals included.
    #[test]
    fn exact_divide_by_factors_matches_the_digit_loop() {
        let mut dividends: Vec<i64> = (-300i64..=300).step_by(7).collect();
        dividends.extend([0, 1, 2, 3, 1_000_000, -999_999_937]);
        let divisors = [
            -40i64, -8, -3, -1, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 25, 40, 64, 80, 96, 99, 125,
            250, 625, 1000, 1024, 3125, 7_812_500, 0,
        ];
        let scales = [
            (0, 0),
            (2, 0),
            (0, 3),
            (-1, 1),
            (i32::MAX, 0),
            (i32::MAX - 1, 0),
            (i32::MAX - 5, 0),
            (i32::MAX, -3),
            (i32::MIN, 0),
            (0, i32::MIN),
            (i32::MIN, 1),
        ];
        let mut checked = 0usize;
        for &a in &dividends {
            for &b in &divisors {
                for &(sa, sb) in &scales {
                    let old = shown(bd_exact_divide_core(&big(a), sa, &big(b), sb));
                    let new = shown(bd_exact_divide_core_factored(&big(a), sa, &big(b), sb));
                    assert_eq!(new, old, "{a}e-{sa} / {b}e-{sb}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 20_000, "vacuous differential: {checked}");
    }

    /// The shape that was ~bits(divisor) passes: a wide divisor with no
    /// factor 2 or 5 refuses at once; a wide power of two terminates at
    /// exactly its exponent.
    #[test]
    fn exact_divide_by_factors_decides_wide_divisors_up_front() {
        let wide_odd = bigint_pow_limbs(&big(3), 20_000);
        assert_eq!(
            shown(bd_exact_divide_core_factored(&big(1), 0, &wide_odd, 0)),
            Err("ArithmeticException: Non-terminating decimal expansion; \
                 no exact representable decimal result."
                .to_string())
        );
        let two_3000 = big(1).shl(3000);
        let want = crate::bigint_pow5(3000).mul(&big(7));
        let got = bd_exact_divide_core_factored(&big(7), 0, &two_3000, 0).expect("terminates");
        assert_eq!(got, (want, 3000));
        let old = bd_exact_divide_core(&big(7), 0, &two_3000, 0).expect("terminates");
        assert_eq!(got, old);
    }

    /// Item 3: `doRound`'s scale arithmetic is `checkScaleNonZero` of a long,
    /// both passes. The message follows the TRUNCATED sign: `(int)(MIN - 1)` is
    /// `MAX`, so it is "Underflow".
    #[test]
    fn round_to_precision_checks_the_scale_in_a_long() {
        let r = |u: i64, s: i32, p: i32| {
            shown(bd_round_to_precision(&big(u), s, p, BD_ROUND_HALF_UP))
        };
        assert_eq!(
            r(123, i32::MIN + 1, 1),
            Err("ArithmeticException: Underflow".to_string())
        );
        // 999 @2 carries into a second pass: MIN + 2 - 1 - 1 = MIN, in range.
        assert_eq!(r(999, i32::MIN + 2, 2), Ok(("10".to_string(), i32::MIN)));
        // ... and one place further the second pass leaves `int`.
        assert_eq!(
            r(999, i32::MIN + 1, 2),
            Err("ArithmeticException: Underflow".to_string())
        );
        assert_eq!(
            r(123_456, i32::MAX, 3),
            Ok(("123".to_string(), i32::MAX - 3))
        );
    }

    fn old_add_mc(
        ua: &BigInt,
        sa: i32,
        ub: &BigInt,
        sb: i32,
        p: i32,
        mode: i32,
    ) -> Result<(BigInt, i32), MethodCallFailed> {
        let s = sa.max(sb);
        let sum = bd_rescale_operand(ua, i64::from(s) - i64::from(sa))?
            .add(&bd_rescale_operand(ub, i64::from(s) - i64::from(sb))?);
        bd_round_to_precision(&sum, s, p, mode)
    }

    /// The JDK's zero-operand and `preAlign` roads give the value (and scale)
    /// the exact-sum-then-round body gave, wherever that body could compute
    /// it.
    #[test]
    fn add_mc_core_agrees_with_exact_then_round() {
        let values = [
            -99_999i64, -1000, -123, -10, -1, 0, 1, 7, 10, 999, 12_345, 100_001,
        ];
        let scales = [-3, 0, 2, 5, 12, 25];
        let mut checked = 0usize;
        for &a in &values {
            for &b in &values {
                for &sa in &scales {
                    for &sb in &scales {
                        for p in 1..=6 {
                            for &m in &MODES {
                                let (ua, ub) = (big(a), big(b));
                                let new = shown(bd_add_mc_core(&ua, sa, &ub, sb, p, m));
                                let old = shown(old_add_mc(&ua, sa, &ub, sb, p, m));
                                assert_eq!(new, old, "{a}e-{sa} + {b}e-{sb} @{p} mode {m}");
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 100_000, "vacuous differential: {checked}");
    }

    /// Scale gaps the old body multiplied out (or refused), answered by the
    /// JDK algorithm: `ONE.add(1E-100000000, DECIMAL64)` is `1.000000000000000`;
    /// UP rounds the invisible tail up; a gap past `bigTenToThe`'s range still
    /// answers; a zero operand keeps only the scale the precision allows.
    #[test]
    fn add_mc_core_answers_huge_scale_gaps_without_the_power_of_ten() {
        let one = big(1);
        let ten15 = "1000000000000000".to_string();
        let add = |a: &BigInt, sa: i32, b: &BigInt, sb: i32, p: i32, m: i32| {
            shown(bd_add_mc_core(a, sa, b, sb, p, m))
        };
        assert_eq!(
            add(&one, 0, &one, 100_000_000, 16, BD_ROUND_HALF_EVEN),
            Ok((ten15.clone(), 15))
        );
        assert_eq!(
            add(&one, 0, &one, 100_000_000, 16, BD_ROUND_UP),
            Ok(("1000000000000001".to_string(), 15))
        );
        assert_eq!(
            add(&one, 0, &one, 2_000_000_000, 16, BD_ROUND_HALF_EVEN),
            Ok((ten15.clone(), 15))
        );
        assert!(old_add_mc(&one, 0, &one, 2_000_000_000, 16, BD_ROUND_HALF_EVEN).is_err());
        // subtract: 1 - 1E-100000000 @16 DOWN is 0.9999999999999999.
        assert_eq!(
            add(&one, 0, &big(-1), 100_000_000, 16, BD_ROUND_DOWN),
            Ok(("9999999999999999".to_string(), 16))
        );
        // ZERO at scale 10^9 plus ONE @16: 1.000000000000000.
        let zero = BigInt::zero();
        assert_eq!(
            add(&zero, 1_000_000_000, &one, 0, 16, BD_ROUND_HALF_EVEN),
            Ok((ten15, 15))
        );
        // 0.00 + 1.5 @5 = 1.50; 1 + 0.00000 @3 = 1.00.
        assert_eq!(
            add(&zero, 2, &big(15), 1, 5, BD_ROUND_HALF_UP),
            Ok(("150".to_string(), 2))
        );
        assert_eq!(
            add(&one, 0, &zero, 5, 3, BD_ROUND_HALF_UP),
            Ok(("100".to_string(), 2))
        );
    }

    fn npe_message(r: Result<Option<Value>, MethodCallFailed>) -> Option<String> {
        match r {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { message },
            ))) => message,
            _ => None,
        }
    }

    /// Item 4: a null operand or `MathContext` is the JDK's NPE, not a null
    /// result; `CRATONVM_BIGDECIMAL_NULL_ARG_NPE=0` restores the old answers.
    #[test]
    fn null_arguments_throw_the_jdk_npe() {
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(0xB16D_EC20);
        let this = ctx.create_string("receiver");
        let other = ctx.create_string("operand");
        let null = Value::Object(None);
        let recv = Value::Object(Some(this));
        let args = [recv, Value::Object(Some(other)), null];
        assert_eq!(
            npe_message(native_bd_add_mc(&mut ctx, &args)).as_deref(),
            Some("Cannot read field \"precision\" because \"mc\" is null")
        );
        assert_eq!(
            npe_message(native_bd_compare_to_exact(&mut ctx, &[recv, null])).as_deref(),
            Some("Cannot read field \"scale\" because \"val\" is null")
        );
        assert_eq!(
            npe_message(native_bd_divide_exact(&mut ctx, &[recv, null])).as_deref(),
            Some("Cannot invoke \"java.math.BigDecimal.signum()\" because \"divisor\" is null")
        );
        let legacy = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_BIGDECIMAL_NULL_ARG_NPE", Some("0"))],
            || {
                let mut ctx = mock_ctx();
                ctx.set_vm_identity(0xB16D_EC21);
                let this = ctx.create_string("receiver");
                let recv = Value::Object(Some(this));
                (
                    native_bd_compare_to_exact(&mut ctx, &[recv, null]).ok(),
                    native_bd_divide_exact(&mut ctx, &[recv, null]).ok(),
                )
            },
        );
        assert_eq!(
            legacy,
            (Some(Some(Value::Int(0))), Some(Some(Value::Object(None))))
        );
    }

    /// The registration-time switches of this wave choose their bodies.
    #[test]
    fn wave9_switches_choose_the_registered_body() {
        let rows: [(&str, &str, &str, &str, usize, usize); 3] = [
            (
                "CRATONVM_BIGINTEGER_NEGATE_SHARES_MAG",
                "java/math/BigInteger",
                "negate",
                "()Ljava/math/BigInteger;",
                super::native_bi_negate_shared as usize,
                super::native_bi_negate as usize,
            ),
            (
                "CRATONVM_BIGDECIMAL_PRECISION_NO_RENDER",
                "java/math/BigDecimal",
                "precision",
                "()I",
                super::native_bd_precision_counted as usize,
                super::native_bd_precision as usize,
            ),
            (
                "CRATONVM_BIGDECIMAL_VALUEOF_LONG_COMPACT",
                "java/math/BigDecimal",
                "valueOf",
                "(J)Ljava/math/BigDecimal;",
                super::native_bd_value_of_long_compact as usize,
                super::native_bd_value_of_long as usize,
            ),
        ];
        let build = || {
            let mut reg = NativeMethodRegistry::new();
            register_biginteger_arithmetic_overrides(&mut reg);
            register_bigdecimal_arithmetic_overrides(&mut reg);
            reg
        };
        for (switch, class, name, desc, on, off) in rows {
            let reg = build();
            let (cb, _) = reg.find_with_kind(class, name, desc).expect("registered");
            assert_eq!(cb as usize, on, "{switch} default");
            let off_reg =
                cratonvm_types::flags::with_thread_overrides(&[(switch, Some("0"))], build);
            let (cb, _) = off_reg.find_with_kind(class, name, desc).expect("registered");
            assert_eq!(cb as usize, off, "{switch}=0");
        }
    }
}

/// Round 13 wave 12 (lane bigdec3): `doubleValue()`/`floatValue()` in binary.
/// The string road (`bd_to_f64`/`bd_to_f32`, an exact decimal handed to Rust's
/// correctly rounded parser) is the reference: correct rounding has exactly
/// one answer, so the two must agree bit for bit on every input.
#[cfg(test)]
mod r13w12_bigdec3_tests {
    use super::{
        bd_compare_by_bits, bd_compare_unscaled, bd_to_f32, bd_to_f32_binary, bd_to_f64,
        bd_to_f64_binary, bigint_decimal_digit_count, bigint_pow10, native_bd_double_value,
        native_bd_double_value_binary, register_bigdecimal_arithmetic_overrides,
    };
    use crate::bigint::BigInt;
    use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state ^ (*state >> 29)
    }

    /// `m * 2^e` as the exact `(unscaled, scale)` pair `new BigDecimal(double)`
    /// builds for it.
    fn exact(m: u64, e: i32) -> (BigInt, i32) {
        let mag = BigInt::from_le_words(false, vec![m as u32, (m >> 32) as u32]);
        if e >= 0 {
            (mag.shl(e as u32), 0)
        } else {
            (mag.mul(&crate::bigint_pow5(e.unsigned_abs())), -e)
        }
    }

    fn check64(u: &BigInt, s: i32) -> f64 {
        let want = bd_to_f64(u, s);
        let got = bd_to_f64_binary(u, s);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "doubleValue of {} scale {s}: binary {got:e}, reference {want:e}",
            u.to_decimal()
        );
        got
    }

    fn check32(u: &BigInt, s: i32) -> f32 {
        let want = bd_to_f32(u, s);
        let got = bd_to_f32_binary(u, s);
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "floatValue of {} scale {s}: binary {got:e}, reference {want:e}",
            u.to_decimal()
        );
        got
    }

    /// The HotSpot-measured rows of `double_value_matches_hotspot` /
    /// `float_value_matches_hotspot`, on the binary road.
    #[test]
    fn binary_road_answers_the_measured_rows() {
        let d = |u: &str, s: i32| bd_to_f64_binary(&BigInt::from_decimal(u), s);
        assert_eq!(d("1", i32::MIN), f64::INFINITY);
        assert_eq!(d("-1", i32::MIN), f64::NEG_INFINITY);
        assert_eq!(d("1", i32::MIN + 1), f64::INFINITY);
        assert_eq!(d("1", i32::MAX), 0.0);
        assert!(d("-1", i32::MAX).is_sign_negative());
        assert!(d("0", i32::MIN).is_sign_positive());
        assert!(d("0", 324).is_sign_positive());
        assert_eq!(d("1", -308), 1.0e308);
        assert_eq!(d("1", -309), f64::INFINITY);
        assert_eq!(d("15", -308), f64::INFINITY);
        assert_eq!(d("17976931348623157", -292), 1.7976931348623157e308);
        assert_eq!(d("17976931348623159", -292), f64::INFINITY);
        assert_eq!(d("1", 323), 9.9e-324);
        assert_eq!(d("1", 324), 0.0);
        assert_eq!(d("15", 324), 1.5e-323);
        assert_eq!(d("49", 326), 0.0);
        assert_eq!(d("1", 325), 0.0);
        assert!(d("-1", 324).is_sign_negative());
        assert_eq!(d("1", 1), 0.1);
        assert_eq!(d("1", -1), 10.0);
        assert_eq!(d("-15", -1), -150.0);
        assert_eq!(d("9007199254740993", 0), 9.007199254740992e15);
        assert_eq!(d("9007199254740993", 324), 9.007199254740994e-309);
        let f = |u: &str, s: i32| bd_to_f32_binary(&BigInt::from_decimal(u), s);
        assert_eq!(f("1", i32::MIN), f32::INFINITY);
        assert_eq!(f("1", i32::MAX), 0.0);
        assert!(f("-1", i32::MAX).is_sign_negative());
        assert_eq!(f("1", -308), f32::INFINITY);
        assert!(f("-1", 308).is_sign_negative());
        assert_eq!(f("1", 1), 0.1f32);
        assert_eq!(f("-15", -1), -150.0f32);
        assert_eq!(f("9007199254740993", 1), 9.0071994e14f32);
    }

    /// Every double's exact value, the exact midpoint to its successor (the
    /// tie, which must go to even) and the two values one unit of a further
    /// decimal place either side of that midpoint (which must not), normals
    /// and subnormals, both formats, against the reference.
    #[test]
    fn binary_road_rounds_exact_values_and_ties_like_the_reference() {
        let mut st = 0x5eed_b16d_ec03u64;
        let mut checked = 0usize;
        for i in 0..6000u32 {
            let bits: u64 = match i % 4 {
                // Anywhere in the finite range.
                0 => lcg(&mut st) % 0x7fef_ffff_ffff_ffff + 1,
                // Subnormal and just above.
                1 => lcg(&mut st) % 0x0020_0000_0000_0000 + 1,
                // Near the top of the range.
                2 => 0x7fef_ffff_ffff_ffff - lcg(&mut st) % 0x0010_0000_0000_0000,
                // Small integers and simple fractions: the fast path's domain.
                _ => ((lcg(&mut st) % 1_000_000 + 1) as f64 / 1024.0).to_bits(),
            };
            let biased = ((bits >> 52) & 0x7ff) as i32;
            let frac = bits & ((1u64 << 52) - 1);
            let (sig, exp) = if biased == 0 {
                (frac, -1074)
            } else {
                (frac | (1 << 52), biased - 1075)
            };
            if sig == 0 {
                continue;
            }
            let (u, s) = exact(sig, exp);
            assert_eq!(check64(&u, s).to_bits(), f64::from_bits(bits).to_bits());
            check32(&u, s);
            let neg = u.neg_value();
            assert_eq!(check64(&neg, s), -f64::from_bits(bits));
            let (mid, ms) = exact(2 * sig + 1, exp - 1);
            let at = check64(&mid, ms);
            check32(&mid, ms);
            let lower = f64::from_bits(bits);
            let (up_u, up_s) = exact(2 * sig + 2, exp - 1);
            let upper = check64(&up_u, up_s);
            let even = if sig & 1 == 0 { lower } else { upper };
            assert_eq!(at.to_bits(), even.to_bits(), "tie of {lower:e}");
            let ten = BigInt::from_decimal("10");
            let one = BigInt::from_decimal("1");
            let above = mid.mul(&ten).add(&one);
            let below = mid.mul(&ten).sub(&one);
            let (a, b) = (check64(&above, ms + 1), check64(&below, ms + 1));
            assert_eq!(a.to_bits(), upper.to_bits(), "above tie of {lower:e}");
            assert_eq!(b.to_bits(), lower.to_bits(), "below tie of {lower:e}");
            check32(&above, ms + 1);
            check32(&below, ms + 1);
            checked += 1;
        }
        assert!(checked > 5000, "vacuous: {checked}");
    }

    /// `float` midpoints: a tie at `f32` precision goes to even at `f32`
    /// precision (a road through `f64` would round twice).
    #[test]
    fn float_road_rounds_its_own_ties() {
        let mut st = 0xf10a7u64;
        for i in 0..4000u32 {
            let bits = match i % 3 {
                0 => (lcg(&mut st) % 0x7f7f_ffff) as u32 + 1,
                1 => (lcg(&mut st) % 0x0100_0000) as u32 + 1,
                _ => 0x7f7f_ffff - (lcg(&mut st) % 0x0080_0000) as u32,
            };
            let biased = ((bits >> 23) & 0xff) as i32;
            let frac = u64::from(bits & ((1u32 << 23) - 1));
            let (sig, exp) = if biased == 0 {
                (frac, -149)
            } else {
                (frac | (1 << 23), biased - 150)
            };
            if sig == 0 {
                continue;
            }
            let (u, s) = exact(sig, exp);
            assert_eq!(check32(&u, s).to_bits(), bits);
            check64(&u, s);
            let (mid, ms) = exact(2 * sig + 1, exp - 1);
            let at = check32(&mid, ms);
            check64(&mid, ms);
            let lower = f32::from_bits(bits);
            let (up_u, up_s) = exact(2 * sig + 2, exp - 1);
            let upper = check32(&up_u, up_s);
            let even = if sig & 1 == 0 { lower } else { upper };
            assert_eq!(at.to_bits(), even.to_bits(), "f32 tie of {lower:e}");
        }
    }

    /// Random operands of 1-40 limbs over a scale window that crosses both
    /// clamps, plus the scale extremes.
    #[test]
    fn binary_road_matches_the_reference_on_random_operands() {
        let mut st = 0xb16_dec3u64;
        let mut checked = 0usize;
        for i in 0..6000u32 {
            let widest = if i % 10 == 0 { 40 } else { 4 };
            let limbs = 1 + (lcg(&mut st) % widest) as usize;
            let words: Vec<u32> = (0..limbs).map(|_| lcg(&mut st) as u32).collect();
            let u = BigInt::from_le_words(i % 3 == 0, words);
            if u.is_zero() {
                continue;
            }
            let digits = (u.magnitude_bits() as f64 * std::f64::consts::LOG10_2) as i64;
            let s = match i % 5 {
                0 => (lcg(&mut st) % 61) as i64 - 30,
                1 => digits + 290 + (lcg(&mut st) % 50) as i64,
                2 => digits - 330 + (lcg(&mut st) % 50) as i64,
                3 => digits + 30 + (lcg(&mut st) % 30) as i64,
                _ => digits - 50 + (lcg(&mut st) % 30) as i64,
            };
            let s = s as i32;
            check64(&u, s);
            check32(&u, s);
            checked += 1;
        }
        let scales = [
            i32::MIN,
            i32::MIN + 1,
            -400,
            -23,
            -22,
            0,
            22,
            23,
            400,
            i32::MAX - 1,
            i32::MAX,
        ];
        let edges = [
            "1",
            "-1",
            "4503599627370495",
            "4503599627370496",
            "4194303",
            "4194304",
            "-9007199254740993",
        ];
        for s in scales {
            for u in edges {
                check64(&BigInt::from_decimal(u), s);
                check32(&BigInt::from_decimal(u), s);
            }
        }
        assert!(checked > 5000, "vacuous: {checked}");
    }

    /// A 20 000-digit value divided down to the double range is one division,
    /// not a 20 000-digit rendering; the answer still matches the reference.
    #[test]
    fn binary_road_handles_wide_values() {
        let wide = crate::bigint_pow5(28_000).mul(&BigInt::from_decimal("7"));
        let digits = (wide.magnitude_bits() as f64 * std::f64::consts::LOG10_2) as i32;
        for s in [digits - 300, digits - 1, digits, digits + 1, digits + 300, digits + 330] {
            check64(&wide, s);
            check32(&wide, s);
        }
    }

    /// The in-register digit count of one- and two-limb magnitudes is the
    /// rendered length, at every power-of-ten boundary and at `u64::MAX`.
    #[test]
    fn small_digit_count_matches_the_rendering() {
        let mut values: Vec<u64> = vec![1, 9, u64::MAX, u64::MAX - 1, u64::from(u32::MAX), 1 << 32];
        let mut p = 1u64;
        while let Some(next) = p.checked_mul(10) {
            values.extend([p - 1, p, p + 1]);
            p = next;
        }
        values.extend([p - 1, p, p + 1]);
        for v in values {
            for neg in [false, true] {
                let b = BigInt::from_le_words(neg, vec![v as u32, (v >> 32) as u32]);
                if b.is_zero() {
                    continue;
                }
                let want = b.to_decimal().trim_start_matches('-').len() as i32;
                assert_eq!(bigint_decimal_digit_count(&b), want, "{v} neg={neg}");
            }
        }
        // Three limbs take the bit-length bracket or the power-of-ten compare.
        let wide = BigInt::from_decimal("100000000000000000000");
        assert_eq!(bigint_decimal_digit_count(&wide), 21);
        assert_eq!(bigint_decimal_digit_count(&BigInt::zero()), 1);
    }

    /// The bit-length bracket of wide magnitudes: every power of ten and its
    /// neighbours up to 10^1500, and random widths, against the rendering.
    #[test]
    fn wide_digit_count_matches_the_rendering() {
        let one = BigInt::from_decimal("1");
        for k in 19..1500u32 {
            let p = bigint_pow10(k);
            for v in [p.sub(&one), p.clone(), p.add(&one)] {
                let want = v.to_decimal().len() as i32;
                assert_eq!(bigint_decimal_digit_count(&v), want, "10^{k} neighbour");
                assert_eq!(bigint_decimal_digit_count(&v.neg_value()), want, "-10^{k} neighbour");
            }
        }
        let mut st = 0xd161_75u64;
        for _ in 0..3000 {
            let limbs = 3 + (lcg(&mut st) % 120) as usize;
            let mut words: Vec<u32> = (0..limbs).map(|_| lcg(&mut st) as u32).collect();
            // Vary the top limb's width too.
            let top = (lcg(&mut st) % 32) as u32;
            if let Some(w) = words.last_mut() {
                *w = (*w >> top).max(1);
            }
            let v = BigInt::from_le_words(false, words);
            assert_eq!(bigint_decimal_digit_count(&v), v.to_decimal().len() as i32);
        }
    }

    /// `compareTo` from bit lengths never disagrees with the exact road, and
    /// decides most operand pairs of different magnitudes.
    #[test]
    fn compare_by_bits_agrees_with_the_exact_road() {
        let mut st = 0xc0a1_bb175u64;
        let (mut decided, mut total) = (0usize, 0usize);
        for i in 0..20_000u32 {
            let widest: u64 = if i % 7 == 0 { 30 } else { 3 };
            let operand = |st: &mut u64| {
                let limbs = 1 + (lcg(st) % widest) as usize;
                let words: Vec<u32> =
                    (0..limbs).map(|_| (lcg(st) as u32) >> (lcg(st) % 32)).collect();
                let v = BigInt::from_le_words(lcg(st) % 5 == 0, words);
                let s = (lcg(st) % 801) as i32 - 400;
                (v, s)
            };
            let (ua, sa) = operand(&mut st);
            let (ub, sb) = operand(&mut st);
            total += 1;
            if let Some(o) = bd_compare_by_bits(&ua, sa, &ub, sb) {
                decided += 1;
                assert_eq!(
                    o,
                    bd_compare_unscaled(&ua, sa, &ub, sb),
                    "{}e-{sa} vs {}e-{sb}",
                    ua.to_decimal(),
                    ub.to_decimal()
                );
            }
        }
        // Scale extremes: the interval arithmetic is in f64, never i32.
        let one = BigInt::from_decimal("1");
        let neg = BigInt::from_decimal("-7");
        for (sa, sb) in [(i32::MIN, i32::MAX), (i32::MAX, i32::MIN), (i32::MIN, 0), (0, i32::MAX)] {
            for (a, b) in [(&one, &one), (&neg, &neg)] {
                if let Some(o) = bd_compare_by_bits(a, sa, b, sb) {
                    assert_eq!(o, bd_compare_unscaled(a, sa, b, sb), "{sa} vs {sb}");
                }
            }
        }
        assert!(decided * 3 > total, "compare_by_bits decided only {decided} of {total}");
    }

    /// `CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE` chooses the registered body; the
    /// kind stays the block's `Intrinsic`.
    #[test]
    fn binary_to_double_switch_chooses_the_registered_body() {
        let build = || {
            let mut reg = NativeMethodRegistry::new();
            register_bigdecimal_arithmetic_overrides(&mut reg);
            reg
        };
        let reg = build();
        let (cb, kind) = reg
            .find_with_kind("java/math/BigDecimal", "doubleValue", "()D")
            .expect("registered");
        assert_eq!(cb as usize, native_bd_double_value_binary as usize);
        assert_eq!(kind, NativeKind::Intrinsic);
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE", Some("0"))],
            build,
        );
        let (cb, kind) = off
            .find_with_kind("java/math/BigDecimal", "doubleValue", "()D")
            .expect("registered");
        assert_eq!(cb as usize, native_bd_double_value as usize);
        assert_eq!(kind, NativeKind::Intrinsic);
    }
}

/// Round 13 wave 13 (lane bigdec4): BD3-1 (`floatValue()` registered in the
/// shipping registrar), BD3-2 (the `5^n` memo) and BD3-3 (which object a JDK
/// 25 `MathContext` road answers).
#[cfg(test)]
mod r13w13_bigdec4_tests {
    use super::{
        bd_add_mc_road, bd_divide_mc_road, bd_mc_rounds, bd_quotient_cmp, bigint_pow5_memo_in,
        native_bd_float_value_binary, register_bigdecimal_arithmetic_overrides, BdMcAnswer,
        Pow5Memo,
    };
    use crate::bigint::BigInt;
    use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
    use std::cmp::Ordering;

    fn big(v: i64) -> BigInt {
        BigInt::from_decimal(&v.to_string())
    }

    fn build() -> NativeMethodRegistry {
        let mut reg = NativeMethodRegistry::new();
        register_bigdecimal_arithmetic_overrides(&mut reg);
        reg
    }

    #[test]
    fn float_value_is_registered_with_the_binary_body() {
        let (cb, kind) = build()
            .find_with_kind("java/math/BigDecimal", "floatValue", "()F")
            .expect("registered");
        assert_eq!(cb as usize, native_bd_float_value_binary as usize);
        assert_eq!(kind, NativeKind::Intrinsic);
        for switch in [
            "CRATONVM_BIGDECIMAL_FLOAT_VALUE_NATIVE",
            "CRATONVM_BIGDECIMAL_BINARY_TO_DOUBLE",
        ] {
            let off = cratonvm_types::flags::with_thread_overrides(&[(switch, Some("0"))], build);
            assert!(
                off.find_with_kind("java/math/BigDecimal", "floatValue", "()F").is_none(),
                "{switch}=0 leaves the JDK bytecode"
            );
        }
    }

    #[test]
    fn pow5_memo_answers_the_power_in_both_arms() {
        let mut memo = Pow5Memo {
            on: None,
            slots: Vec::new(),
        };
        // Twice over, so the second pass reads the slots the first filled;
        // `n` and `n + 64` share a slot and evict each other.
        for pass in 0..2 {
            for n in (0..=2300u32).step_by(7).chain([63, 127, 64, 128, 2199, 2200, 2201]) {
                let want = crate::bigint_pow5(n);
                assert_eq!(*bigint_pow5_memo_in(&mut memo, true, n), want, "on, n {n}, pass {pass}");
                assert_eq!(*bigint_pow5_memo_in(&mut memo, false, n), want, "off, n {n}");
            }
        }
        assert!(memo.slots.len() == super::POW5_MEMO_SLOTS);
    }

    #[test]
    fn digit_drop_is_decided_by_the_precision() {
        assert!(!bd_mc_rounds(&big(999), 3));
        assert!(bd_mc_rounds(&big(1000), 3));
        assert!(bd_mc_rounds(&big(-1000), 3));
        assert!(!bd_mc_rounds(&big(123_456), 0));
        assert!(!bd_mc_rounds(&BigInt::zero(), 1));
    }

    /// `add(augend, mc)` (`subtract` negates the augend first), JDK 25:
    /// `ZERO.add(x, mc)` is `x` itself, `x.add(ZERO, mc)` is the receiver,
    /// a digit drop is a fresh object, a raise to the preferred scale and
    /// the both-non-zero road are `valueOf`s.
    #[test]
    fn add_mc_roads() {
        let z = BigInt::zero();
        // 0 (scale 0) + 12.5 (scale 1), precision 16: preferred 1 = x's scale.
        assert_eq!(bd_add_mc_road((&z, 0), (&big(125), 1), 16, 1, false), BdMcAnswer::Operand);
        // 12.5 + 0: the receiver.
        assert_eq!(bd_add_mc_road((&big(125), 1), (&z, 0), 16, 1, false), BdMcAnswer::Receiver);
        // subtract: 0 - 12.5 is `(12.5).negate()`, a `valueOf`.
        assert_eq!(bd_add_mc_road((&z, 0), (&big(-125), 1), 16, 1, true), BdMcAnswer::Constant);
        // 12.5 - 0: the receiver.
        assert_eq!(bd_add_mc_road((&big(125), 1), (&z, 0), 16, 1, true), BdMcAnswer::Receiver);
        // 0 (scale 3) + 5 (scale 0), precision 16: raised to 5.000 -- `setScale`.
        assert_eq!(bd_add_mc_road((&z, 3), (&big(5), 0), 16, 3, false), BdMcAnswer::Constant);
        // 0 + 12345 at precision 2: `doRound` drops digits -- fresh.
        assert_eq!(bd_add_mc_road((&z, 0), (&big(12_345), 0), 2, -3, false), BdMcAnswer::Fresh);
        // Both non-zero, both zero, precision 0: `valueOf` roads.
        assert_eq!(bd_add_mc_road((&big(1), 0), (&big(1), 0), 16, 0, false), BdMcAnswer::Constant);
        assert_eq!(bd_add_mc_road((&z, 0), (&z, 2), 16, 2, false), BdMcAnswer::Constant);
        assert_eq!(bd_add_mc_road((&z, 0), (&big(7), 0), 0, 0, false), BdMcAnswer::Constant);
    }

    #[test]
    fn quotient_comparison() {
        // 0.96 / 1 against 1: below.
        assert_eq!(bd_quotient_cmp(&big(96), 2, &big(1), 0, &big(1)), Some(Ordering::Less));
        // 1.4 / 1 against 1: above.
        assert_eq!(bd_quotient_cmp(&big(14), 1, &big(1), 0, &big(1)), Some(Ordering::Greater));
        // 1.0 / 1 and 3 / 0.3 against 1 and 10: equal.
        assert_eq!(bd_quotient_cmp(&big(10), 1, &big(1), 0, &big(1)), Some(Ordering::Equal));
        assert_eq!(bd_quotient_cmp(&big(3), 0, &big(3), 1, &big(10)), Some(Ordering::Equal));
        // Signs are ignored; a scale gap out of proportion is not compared.
        assert_eq!(bd_quotient_cmp(&big(-96), 2, &big(1), 0, &big(1)), Some(Ordering::Less));
        assert_eq!(bd_quotient_cmp(&big(1), 1000, &big(1), 0, &big(1)), None);
    }

    /// `divide(divisor, mc)`: only a `1` (precision 1) or `10` (precision 2) at
    /// scale 0 can be `doRound`'s fresh object.
    #[test]
    fn divide_mc_roads() {
        let one = (&big(1), 0);
        // ONE.divide(ONE, precision 1): the stripped quotient is `valueOf(1, 0)`.
        assert_eq!(bd_divide_mc_road((&big(1), 0), (&big(1), 0), 1, one), BdMcAnswer::Constant);
        // new BigDecimal("1.0").divide(ONE, precision 1): the quotient `1.0`
        // keeps scale 1 (preferred), `doRound` makes a fresh `1`.
        assert_eq!(bd_divide_mc_road((&big(10), 1), (&big(1), 0), 1, one), BdMcAnswer::Fresh);
        // 0.96 / 1 at precision 1 rounds UP across the decade: fresh.
        assert_eq!(bd_divide_mc_road((&big(96), 2), (&big(1), 0), 1, one), BdMcAnswer::Fresh);
        // 1.4 / 1 at precision 1 rounds down to `valueOf(1, 0)`.
        assert_eq!(bd_divide_mc_road((&big(14), 1), (&big(1), 0), 1, one), BdMcAnswer::Constant);
        // 30 / 3 at precision 2: exact, preferred 0.
        assert_eq!(
            bd_divide_mc_road((&big(30), 0), (&big(3), 0), 2, (&big(10), 0)),
            BdMcAnswer::Constant
        );
        // Values that `doRound` cannot produce are always the `valueOf` road.
        assert_eq!(
            bd_divide_mc_road((&big(7), 0), (&big(1), 0), 1, (&big(7), 0)),
            BdMcAnswer::Constant
        );
        assert_eq!(
            bd_divide_mc_road((&big(2), 0), (&big(3), 0), 16, (&big(6_666_666_666_666_667), 16)),
            BdMcAnswer::Constant
        );
    }
}

/// Round 14 wave 2 (lane bigdec): BD4-2's memoised `bi * 10^n`, and the limb
/// roads of the decimal-string `gcd` / `toByteArray` helpers, against
/// independent references.
#[cfg(test)]
mod r14w2_bigdec_tests {
    use super::{bi_gcd_str, bi_to_byte_array_int, bi_to_byte_array_str, bigint_mul_pow10_memo};
    use crate::bigint::BigInt;

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 11
    }

    fn random(state: &mut u64, words: usize, neg: bool) -> BigInt {
        let mag: Vec<u32> = (0..words).map(|_| lcg(state) as u32).collect();
        BigInt::from_le_words(neg, mag)
    }

    #[test]
    fn mul_pow10_memo_is_the_uncached_product() {
        let mut st = 0x10_u64;
        let values = [
            BigInt::zero(),
            BigInt::from_decimal("1"),
            BigInt::from_decimal("-1"),
            random(&mut st, 3, false),
            random(&mut st, 7, true),
            random(&mut st, 90, false),
        ];
        let ns = (0u32..64).chain([27, 28, 100, 511, 1000, 2199, 2200, 2201, 3000]);
        for n in ns {
            let p = crate::bigint_pow5(n).shl(n);
            for v in &values {
                // Twice: the second call is served from the memo.
                assert_eq!(bigint_mul_pow10_memo(v, n), v.mul(&p), "n {n}");
                assert_eq!(bigint_mul_pow10_memo(v, n), v.mul(&p), "n {n}, memo");
            }
            assert_eq!(crate::bigint_mul_pow10(&values[4], n as i32), values[4].mul(&p), "lib n {n}");
        }
    }

    #[test]
    fn gcd_limb_road_is_exact() {
        let mut st = 0x9cd_u64;
        for words in [1usize, 2, 3, 5, 9, 17, 40] {
            let g0 = random(&mut st, words, false).add(&BigInt::from_decimal("1"));
            let k = random(&mut st, words + 1, false);
            // `k` and `k + 1` are coprime, so the gcd is exactly `g0`.
            let a = g0.mul(&k);
            let b = g0.mul(&k.add(&BigInt::from_decimal("1")));
            let (sa, sb, sg) = (a.to_decimal(), b.to_decimal(), g0.to_decimal());
            assert_eq!(bi_gcd_str(&sa, &sb), sg, "words {words}");
            assert_eq!(bi_gcd_str(&format!("-{sa}"), &sb), sg, "neg a, words {words}");
            assert_eq!(bi_gcd_str(&sb, &format!("-{sa}")), sg, "swapped, words {words}");
            assert_eq!(bi_gcd_str("0", &sa), sa, "gcd(0, a)");
            assert_eq!(bi_gcd_str(&sa, "0"), sa, "gcd(a, 0)");
            assert_eq!(bi_gcd_str(&sa, &sa), sa, "gcd(a, a)");
        }
        assert_eq!(bi_gcd_str("0", "0"), "0");
        assert_eq!(bi_gcd_str("12", "-18"), "6");
    }

    /// `toByteArray()` by its specification: `bitLength() / 8 + 1` bytes of
    /// the two's-complement value, big-endian.
    fn to_byte_array_spec(v: &BigInt) -> Vec<u8> {
        let len = (v.bit_length() / 8 + 1) as usize;
        let x = if v.is_neg() {
            BigInt::from_decimal("1").shl(8 * len as u32).add(v)
        } else {
            v.clone()
        };
        let mut le: Vec<u8> = x.mag_le().iter().flat_map(|w| w.to_le_bytes()).collect();
        le.resize(len, 0);
        le.reverse();
        le
    }

    #[test]
    fn to_byte_array_limb_road_matches_the_spec() {
        let mut st = 0xb7e5_u64;
        let mut values: Vec<BigInt> = Vec::new();
        for words in 1usize..24 {
            values.push(random(&mut st, words, false));
            values.push(random(&mut st, words, true));
        }
        // Powers of two (the negative ones need one byte less) and their neighbours.
        let one = BigInt::from_decimal("1");
        for bits in [63u32, 64, 65, 127, 128, 255, 256, 257, 511, 1000] {
            let p = one.shl(bits);
            for v in [p.clone(), p.sub(&one), p.add(&one)] {
                values.push(v.neg_value());
                values.push(v);
            }
        }
        for k in -300i64..300 {
            values.push(BigInt::from_decimal(&k.to_string()));
        }
        for v in values {
            let want = to_byte_array_spec(&v);
            assert_eq!(bi_to_byte_array_int(&v), want, "int {}", v.to_decimal());
            if v.is_zero() {
                continue;
            }
            assert_eq!(bi_to_byte_array_str(&v.to_decimal()), want, "{}", v.to_decimal());
        }
    }
}

#[cfg(test)]
mod r14w2_compat2_digit_tests {
    use super::{bi_java_digit, bi_number_format_message, bi_parse_java, BMP_ND_ZEROS};

    fn on() -> bool {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_BIGINT_UNICODE_DIGITS")
    }

    #[test]
    fn ascii_digits_and_letters_follow_character_digit() {
        assert_eq!(bi_java_digit(u16::from(b'7'), 10), Some(7));
        assert_eq!(bi_java_digit(u16::from(b'z'), 36), Some(35));
        assert_eq!(bi_java_digit(u16::from(b'Z'), 36), Some(35));
        assert_eq!(bi_java_digit(u16::from(b'a'), 10), None);
        assert_eq!(bi_java_digit(u16::from(b'_'), 36), None);
        assert_eq!(bi_parse_java("0012", 10).ok(), Some("12".to_string()));
    }

    #[test]
    fn unicode_decimal_digits_are_digits() {
        if !on() {
            return;
        }
        // Every block: ten consecutive digits valued 0..9, nothing past them.
        for (i, &z) in BMP_ND_ZEROS.iter().enumerate() {
            for k in 0..10u16 {
                assert_eq!(bi_java_digit(z + k, 10), Some(u32::from(k)), "U+{:04X}", z + k);
            }
            let next = BMP_ND_ZEROS.get(i + 1).copied().unwrap_or(u16::MAX);
            if z + 10 < next {
                assert_eq!(bi_java_digit(z + 10, 36), None, "U+{:04X}", z + 10);
            }
        }
        assert_eq!(
            bi_parse_java("\u{661}\u{662}\u{663}", 10).ok(),
            Some("123".to_string())
        );
        assert_eq!(
            bi_parse_java("-\u{FF10}\u{FF14}\u{FF12}", 10).ok(),
            Some("-42".to_string())
        );
        assert_eq!(bi_parse_java("\u{FF11}\u{FF10}", 16).ok(), Some("16".to_string()));
        // Fullwidth letters are digits 10..35.
        assert_eq!(bi_parse_java("\u{FF26}\u{FF46}", 16).ok(), Some("255".to_string()));
        assert_eq!(bi_java_digit(0xFF26, 15), None);
    }

    #[test]
    fn other_numerics_and_surrogates_are_not_digits() {
        // VULGAR FRACTION ONE FIFTH (No), ROMAN NUMERAL ONE (Nl), SUPERSCRIPT
        // TWO (No), NEW TAI LUE THAM DIGIT ONE (No), a surrogate.
        for c in [0x2155u16, 0x2160, 0x00B2, 0x19DA, 0xD835] {
            assert_eq!(bi_java_digit(c, 36), None, "U+{c:04X}");
        }
        assert!(bi_parse_java("\u{2155}", 10).is_err());
        // MATHEMATICAL BOLD DIGIT ZERO is `Nd` but supplementary: two chars,
        // neither a digit, as in the JDK.
        assert!(bi_parse_java("\u{1D7CE}", 10).is_err());
    }

    #[test]
    fn the_message_group_counts_utf16_units() {
        let digits: Vec<u16> = "1\u{1D7CE}".encode_utf16().collect();
        assert_eq!(digits.len(), 3);
        let m = bi_number_format_message(&digits, 1, 10);
        assert_eq!(m, "For input string: \"1\u{1D7CE}\"");
    }
}
