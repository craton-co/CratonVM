// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/Math`.
//!
//! See `intrinsic_table_contract.md` and
//! `feature_roadmap_interpreter_intrinsic_table.md`.
//!
//! Hard project rule (`feedback_no_synthetic_stubs`): these handlers MUST be
//! byte-for-byte behaviour-identical to the normal native-registry dispatch
//! path. Unlike the Object/String/etc. groups, the contract explicitly says
//! the `Math` handlers are pure scalar arithmetic and are implemented INLINE —
//! the inline arithmetic IS the real implementation, not a stub. Each handler
//! computes exactly what its `crate::lang_math::native_math_*` counterpart
//! does (same `wrapping_abs` / `std::cmp` / `f64` ops, same operand-decoding
//! idiom), so the intrinsic and the slow path cannot diverge.
//!
//! | (class, name, descriptor)   | InterpIntrinsic | handler                    |
//! |-----------------------------|-----------------|----------------------------|
//! | `java/lang/Math abs (I)I`   | `MathAbsInt`    | `intrinsic_math_abs_int`   |
//! | `java/lang/Math abs (J)J`   | `MathAbsLong`   | `intrinsic_math_abs_long`  |
//! | `java/lang/Math abs (D)D`   | `MathAbsDouble` | `intrinsic_math_abs_double`|
//! | `java/lang/Math min (II)I`  | `MathMinInt`    | `intrinsic_math_min_int`   |
//! | `java/lang/Math max (II)I`  | `MathMaxInt`    | `intrinsic_math_max_int`   |
//! | `java/lang/Math min (JJ)J`  | `MathMinLong`   | `intrinsic_math_min_long`  |
//! | `java/lang/Math max (JJ)J`  | `MathMaxLong`   | `intrinsic_math_max_long`  |
//! | `java/lang/Math sqrt (D)D`  | `MathSqrt`      | `intrinsic_math_sqrt`      |
//!
//! All eight methods are STATIC, so `args` layout is `[param0, ...]` — no
//! receiver. `ctx` is unused (pure arithmetic, no heap access).
//!
//! Structure: each handler decodes its operands from `args` and delegates to a
//! private `compute_*` pure `fn`. The `compute_*` fns hold the actual Java
//! semantics and are exercised directly by the `#[cfg(test)]` module — a leaf
//! crate cannot construct a live `NativeContext`, so testing the pure cores is
//! how edge-case coverage is achieved here.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// Operand decoding — identical idiom to `crate::lang_math`.
// ---------------------------------------------------------------------------

/// Extract an `i32` from `args[idx]`, mirroring the `lang_math.rs` idiom
/// (`match args.first() { Some(Value::Int(v)) => *v, _ => 0 }`).
#[inline(always)]
fn int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

/// Extract an `i64` from `args[idx]`.
///
/// This reproduces `lang_math::long_arg` exactly: long arguments crossing the
/// native-invocation boundary may arrive tagged as `Double` (CompactValue
/// stores untagged 64-bit values whose `tag()` returns `Double` whenever the
/// bit-pattern doesn't collide with a NaN-tag); reinterpret bits to recover
/// the original `i64`. Keeping this identical to the native path is required
/// for byte-for-byte parity with the slow dispatch path.
#[inline(always)]
fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

/// Extract an `f64` from `args[idx]`, mirroring the `lang_math.rs` idiom.
#[inline(always)]
fn double_arg(args: &[Value], idx: usize) -> f64 {
    match args.get(idx) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Pure computation cores — these encode the Java semantics.
// ---------------------------------------------------------------------------

/// `Math.abs(int)` — `Math.abs(Integer.MIN_VALUE)` returns `Integer.MIN_VALUE`
/// (silent overflow); `i32::wrapping_abs` reproduces that exactly.
#[inline(always)]
fn compute_abs_int(v: i32) -> i32 {
    v.wrapping_abs()
}

/// `Math.abs(long)` — `Math.abs(Long.MIN_VALUE)` returns `Long.MIN_VALUE`
/// (silent overflow); `i64::wrapping_abs` reproduces that exactly.
#[inline(always)]
fn compute_abs_long(v: i64) -> i64 {
    v.wrapping_abs()
}

/// `Math.abs(double)` — clears the sign bit: `abs(-0.0) == +0.0`, `abs(NaN)`
/// is NaN, `abs(-Infinity) == +Infinity`. `f64::abs` has exactly those
/// semantics.
#[inline(always)]
fn compute_abs_double(v: f64) -> f64 {
    v.abs()
}

/// `Math.min(int, int)`.
#[inline(always)]
fn compute_min_int(a: i32, b: i32) -> i32 {
    std::cmp::min(a, b)
}

/// `Math.max(int, int)`.
#[inline(always)]
fn compute_max_int(a: i32, b: i32) -> i32 {
    std::cmp::max(a, b)
}

/// `Math.min(long, long)`.
#[inline(always)]
fn compute_min_long(a: i64, b: i64) -> i64 {
    std::cmp::min(a, b)
}

/// `Math.max(long, long)`.
#[inline(always)]
fn compute_max_long(a: i64, b: i64) -> i64 {
    std::cmp::max(a, b)
}

/// `Math.sqrt(double)` — matches Java's `Math.sqrt`/`StrictMath.sqrt`:
/// `sqrt` of a negative value is NaN, `sqrt(-0.0) == -0.0`,
/// `sqrt(+0.0) == +0.0`, `sqrt(+Infinity) == +Infinity`. `f64::sqrt` already
/// has these semantics.
#[inline(always)]
fn compute_sqrt(v: f64) -> f64 {
    v.sqrt()
}

// ---------------------------------------------------------------------------
// Intrinsic handlers (NativeCallback-compatible).
// ---------------------------------------------------------------------------

/// Intrinsic for `java/lang/Math.abs (I)I` (static).
#[inline(always)]
pub fn intrinsic_math_abs_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(compute_abs_int(int_arg(args, 0)))))
}

/// Intrinsic for `java/lang/Math.abs (J)J` (static).
#[inline(always)]
pub fn intrinsic_math_abs_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(compute_abs_long(long_arg(args, 0)))))
}

/// Intrinsic for `java/lang/Math.abs (D)D` (static).
#[inline(always)]
pub fn intrinsic_math_abs_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Double(compute_abs_double(double_arg(args, 0)))))
}

/// Intrinsic for `java/lang/Math.min (II)I` (static).
#[inline(always)]
pub fn intrinsic_math_min_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(compute_min_int(
        int_arg(args, 0),
        int_arg(args, 1),
    ))))
}

/// Intrinsic for `java/lang/Math.max (II)I` (static).
#[inline(always)]
pub fn intrinsic_math_max_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(compute_max_int(
        int_arg(args, 0),
        int_arg(args, 1),
    ))))
}

/// Intrinsic for `java/lang/Math.min (JJ)J` (static).
#[inline(always)]
pub fn intrinsic_math_min_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(compute_min_long(
        long_arg(args, 0),
        long_arg(args, 1),
    ))))
}

/// Intrinsic for `java/lang/Math.max (JJ)J` (static).
#[inline(always)]
pub fn intrinsic_math_max_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(compute_max_long(
        long_arg(args, 0),
        long_arg(args, 1),
    ))))
}

/// Intrinsic for `java/lang/Math.sqrt (D)D` (static).
#[inline(always)]
pub fn intrinsic_math_sqrt(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Double(compute_sqrt(double_arg(args, 0)))))
}

#[cfg(test)]
mod tests;
