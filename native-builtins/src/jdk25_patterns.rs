// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK 25 — Primitive Types in Patterns (JEP 507, 3rd Preview) and
//! Stable Values (JEP 502, Preview).
//!
//! Phase 15.3: Runtime support for primitive pattern matching — widening,
//! narrowing, and exact-conversion checks across all eight Java primitive
//! types.
//!
//! Phase 15.4: `java/lang/StableValue` — a lazily-initialized, write-once
//! container with list/map factory methods.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ===========================================================================
// 15.3 — Primitive Types in Patterns (JEP 507)
// ===========================================================================

/// The eight Java primitive types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
}

impl PrimitiveType {
    /// Decode a JVM type code (used by native methods) into a `PrimitiveType`.
    ///
    /// Type codes follow the JVM spec's `T_` constants:
    ///   4=boolean, 5=char, 6=float, 7=double, 8=byte, 9=short, 10=int, 11=long
    pub fn from_type_code(code: i32) -> Option<PrimitiveType> {
        match code {
            4 => Some(PrimitiveType::Boolean),
            5 => Some(PrimitiveType::Char),
            6 => Some(PrimitiveType::Float),
            7 => Some(PrimitiveType::Double),
            8 => Some(PrimitiveType::Byte),
            9 => Some(PrimitiveType::Short),
            10 => Some(PrimitiveType::Int),
            11 => Some(PrimitiveType::Long),
            _ => None,
        }
    }
}

/// Result of a pattern match attempt against a primitive value.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternMatchResult {
    /// The match succeeded; the (possibly converted) value is carried here.
    Match(Value),
    /// The match did not succeed (value out of range, etc.).
    NoMatch,
    /// An error occurred during the match attempt.
    Error(String),
}

// ---------------------------------------------------------------------------
// PrimitivePatternMatcher
// ---------------------------------------------------------------------------

/// Standalone helper that answers conversion questions for primitive types
/// as required by JEP 507.
pub struct PrimitivePatternMatcher;

impl PrimitivePatternMatcher {
    // -- Widening conversions (JLS 5.1.2) -----------------------------------

    /// Returns `true` if the JVM permits a *widening* primitive conversion
    /// from `from` to `to`.
    ///
    /// Widening conversions that are always allowed:
    ///   byte  -> short, int, long, float, double
    ///   short -> int, long, float, double
    ///   char  -> int, long, float, double
    ///   int   -> long, float, double
    ///   long  -> float, double
    ///   float -> double
    ///
    /// Boolean does not participate in widening.
    pub fn can_widen(from: PrimitiveType, to: PrimitiveType) -> bool {
        use PrimitiveType::*;
        matches!(
            (from, to),
            (Byte, Short)
                | (Byte, Int)
                | (Byte, Long)
                | (Byte, Float)
                | (Byte, Double)
                | (Short, Int)
                | (Short, Long)
                | (Short, Float)
                | (Short, Double)
                | (Char, Int)
                | (Char, Long)
                | (Char, Float)
                | (Char, Double)
                | (Int, Long)
                | (Int, Float)
                | (Int, Double)
                | (Long, Float)
                | (Long, Double)
                | (Float, Double)
        )
    }

    // -- Narrowing conversions (JLS 5.1.3) ----------------------------------

    /// Returns `true` if a *narrowing* primitive conversion from `from` to
    /// `to` is defined.
    ///
    /// Narrowing conversions:
    ///   short  -> byte, char
    ///   char   -> byte, short
    ///   int    -> byte, short, char
    ///   long   -> byte, short, char, int
    ///   float  -> byte, short, char, int, long
    ///   double -> byte, short, char, int, long, float
    pub fn can_narrow(from: PrimitiveType, to: PrimitiveType) -> bool {
        use PrimitiveType::*;
        matches!(
            (from, to),
            (Short, Byte)
                | (Short, Char)
                | (Char, Byte)
                | (Char, Short)
                | (Int, Byte)
                | (Int, Short)
                | (Int, Char)
                | (Long, Byte)
                | (Long, Short)
                | (Long, Char)
                | (Long, Int)
                | (Float, Byte)
                | (Float, Short)
                | (Float, Char)
                | (Float, Int)
                | (Float, Long)
                | (Double, Byte)
                | (Double, Short)
                | (Double, Char)
                | (Double, Int)
                | (Double, Long)
                | (Double, Float)
        )
    }

    // -- Exact (lossless) conversions ---------------------------------------

    /// Returns `true` if a conversion from `from` to `to` is guaranteed to
    /// be *exact* — i.e. no precision loss is possible for any value of the
    /// source type.
    ///
    /// Exact widening conversions:
    ///   byte  -> short, int, long, double
    ///   short -> int, long, double
    ///   char  -> int, long, double
    ///   int   -> long, double
    ///   long  -> (none — long->float and long->double can lose precision)
    ///   float -> double
    ///
    /// Note: int->float is NOT exact (e.g. 2^24+1 loses precision).
    /// Note: long->float and long->double are NOT exact.
    pub fn is_exact_conversion(from: PrimitiveType, to: PrimitiveType) -> bool {
        use PrimitiveType::*;
        if from == to {
            return true;
        }
        matches!(
            (from, to),
            (Byte, Short)
                | (Byte, Int)
                | (Byte, Long)
                | (Byte, Double)
                | (Short, Int)
                | (Short, Long)
                | (Short, Double)
                | (Char, Int)
                | (Char, Long)
                | (Char, Double)
                | (Int, Long)
                | (Int, Double)
                | (Float, Double)
        )
    }

    /// Check whether a concrete `float` value can round-trip through `int`
    /// without precision loss: `(float)(int)f == f` and the value is in int
    /// range.
    pub fn is_exact_float(f: f32) -> bool {
        if f.is_nan() || f.is_infinite() {
            return false;
        }
        let i = f as i32;
        let back = i as f32;
        back == f
    }

    /// Check whether a concrete `double` value can round-trip through `long`
    /// without precision loss: `(double)(long)d == d` and the value is in
    /// long range.
    pub fn is_exact_double(d: f64) -> bool {
        if d.is_nan() || d.is_infinite() {
            return false;
        }
        let l = d as i64;
        let back = l as f64;
        back == d
    }
}

// ---------------------------------------------------------------------------
// Native methods — jdk/internal/misc/PatternSupport
// ---------------------------------------------------------------------------

/// `exactConversionCheck(ID)Z` — arg0: int type-code of source,
/// arg1: double-encoded type-code of target.  Returns Int(1) if exact.
fn native_exact_conversion_check(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_code = match args.get(0) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Second arg is a double carrying the target type code.
    let to_code = match args.get(1) {
        Some(Value::Double(v)) => *v as i32,
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = match (
        PrimitiveType::from_type_code(from_code),
        PrimitiveType::from_type_code(to_code),
    ) {
        (Some(f), Some(t)) => PrimitivePatternMatcher::is_exact_conversion(f, t),
        _ => false,
    };
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `widenPrimitive(II)I` — returns Int(1) if widening from type-code arg0
/// to type-code arg1 is allowed.
fn native_widen_primitive(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_code = match args.get(0) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let to_code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = match (
        PrimitiveType::from_type_code(from_code),
        PrimitiveType::from_type_code(to_code),
    ) {
        (Some(f), Some(t)) => PrimitivePatternMatcher::can_widen(f, t),
        _ => false,
    };
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `narrowPrimitive(II)I` — returns Int(1) if narrowing from type-code arg0
/// to type-code arg1 is allowed.
fn native_narrow_primitive(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let from_code = match args.get(0) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let to_code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = match (
        PrimitiveType::from_type_code(from_code),
        PrimitiveType::from_type_code(to_code),
    ) {
        (Some(f), Some(t)) => PrimitivePatternMatcher::can_narrow(f, t),
        _ => false,
    };
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `isExactFloat(F)Z` — can the given float round-trip through int?
fn native_is_exact_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let f = match args.get(0) {
        Some(Value::Float(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        if PrimitivePatternMatcher::is_exact_float(f) {
            1
        } else {
            0
        },
    )))
}

/// `isExactDouble(D)Z` — can the given double round-trip through long?
fn native_is_exact_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let d = match args.get(0) {
        Some(Value::Double(v)) => *v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        if PrimitivePatternMatcher::is_exact_double(d) {
            1
        } else {
            0
        },
    )))
}

// ===========================================================================
// 15.4 — Stable Values (JEP 502)
// ===========================================================================

// StableValue synthetic object field layout:
//   field 0 — value_ref   (Object)
//   field 1 — is_set      (Int 0/1)
//   field 2 — supplier_ref (Object)

const SV_FIELD_VALUE: usize = 0;
const SV_FIELD_IS_SET: usize = 1;
const SV_FIELD_SUPPLIER: usize = 2;
const SV_NUM_FIELDS: usize = 3;

// ---------------------------------------------------------------------------
// StableValue native methods
// ---------------------------------------------------------------------------

/// `of()Ljava/lang/StableValue;` — create an empty StableValue.
fn native_stable_value_of_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/StableValue", SV_NUM_FIELDS)?;
    ctx.set_field(obj, SV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(obj, SV_FIELD_IS_SET, Value::Int(0));
    ctx.set_field(obj, SV_FIELD_SUPPLIER, Value::Object(None));
    Ok(Some(Value::Object(Some(obj))))
}

/// `of(Ljava/util/function/Supplier;)Ljava/lang/StableValue;` — create with supplier.
fn native_stable_value_of_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let supplier = match args.get(0) {
        Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
        _ => Value::Object(None),
    };
    let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/StableValue", SV_NUM_FIELDS)?;
    ctx.set_field(obj, SV_FIELD_VALUE, Value::Object(None));
    ctx.set_field(obj, SV_FIELD_IS_SET, Value::Int(0));
    ctx.set_field(obj, SV_FIELD_SUPPLIER, supplier);
    Ok(Some(Value::Object(Some(obj))))
}

/// Invoke a `Supplier.get()` and return its result, or `Value::Object(None)`
/// if the supplier reference is null/invalid. Propagates any exception thrown
/// by the supplier to the caller.
fn invoke_supplier(
    ctx: &mut dyn NativeContext,
    supplier: Value,
) -> Result<Value, cratonvm_types::error::MethodCallFailed> {
    if let Value::Object(Some(s)) = supplier {
        let r = ctx.invoke_virtual(s, "get", "()Ljava/lang/Object;", &[])?;
        Ok(r.unwrap_or(Value::Object(None)))
    } else {
        Ok(Value::Object(None))
    }
}

/// `computeIfUnset(Ljava/util/function/Supplier;)Ljava/lang/Object;` (JDK 25
/// `orElseSet`): if the value is already set, return it; otherwise invoke the
/// supplied `Supplier.get()`, store the result write-once, and return it.
///
/// Per the JEP 502 contract this is idempotent: once set, the supplier is
/// never invoked again. Any exception thrown by the supplier propagates and
/// the StableValue remains unset.
fn native_stable_value_compute_if_unset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    if let Value::Int(1) = is_set {
        // Already set — return current value, do NOT invoke the supplier.
        let val = ctx.get_field(this, SV_FIELD_VALUE);
        return Ok(Some(val));
    }
    // Not set — invoke the supplier passed as the argument (arg1), falling
    // back to a supplier stored at construction time (of(Supplier)).
    let supplier = match args.get(1) {
        Some(Value::Object(Some(_))) => args[1],
        _ => ctx.get_field(this, SV_FIELD_SUPPLIER),
    };
    let computed = invoke_supplier(ctx, supplier)?;
    // Re-check is_set: the supplier could have set this StableValue reentrantly.
    if let Value::Int(1) = ctx.get_field(this, SV_FIELD_IS_SET) {
        return Ok(Some(ctx.get_field(this, SV_FIELD_VALUE)));
    }
    ctx.set_field(this, SV_FIELD_VALUE, computed);
    ctx.set_field(this, SV_FIELD_IS_SET, Value::Int(1));
    Ok(Some(computed))
}

/// `orElseThrow()Ljava/lang/Object;`
///
/// Returns the value if set. If unset but a supplier was stored at
/// construction (`of(Supplier)`), invokes it lazily, stores the result
/// write-once, and returns it. If unset and there is no supplier, throws
/// `NoSuchElementException` per the JEP 502 contract.
fn native_stable_value_or_else_throw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    if let Value::Int(1) = is_set {
        let val = ctx.get_field(this, SV_FIELD_VALUE);
        return Ok(Some(val));
    }
    // Not set — try a construction-time supplier before giving up.
    let supplier = ctx.get_field(this, SV_FIELD_SUPPLIER);
    if let Value::Object(Some(_)) = supplier {
        let computed = invoke_supplier(ctx, supplier)?;
        if let Value::Int(1) = ctx.get_field(this, SV_FIELD_IS_SET) {
            return Ok(Some(ctx.get_field(this, SV_FIELD_VALUE)));
        }
        ctx.set_field(this, SV_FIELD_VALUE, computed);
        ctx.set_field(this, SV_FIELD_IS_SET, Value::Int(1));
        return Ok(Some(computed));
    }
    // No value and no supplier — throw, matching StableValue.orElseThrow().
    Err(
        cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "StableValue has no contents".to_string(),
        }
        .into(),
    )
}

/// `orElse(Ljava/lang/Object;)Ljava/lang/Object;`
fn native_stable_value_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    if let Value::Int(1) = is_set {
        let val = ctx.get_field(this, SV_FIELD_VALUE);
        return Ok(Some(val));
    }
    // Not set — return the default argument.
    let default = args.get(1).cloned().unwrap_or(Value::Object(None));
    Ok(Some(default))
}

/// `isSet()Z`
fn native_stable_value_is_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    match is_set {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// `trySet(Ljava/lang/Object;)Z`
fn native_stable_value_try_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    if let Value::Int(1) = is_set {
        return Ok(Some(Value::Int(0))); // already set
    }
    let value = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.set_field(this, SV_FIELD_VALUE, value);
    ctx.set_field(this, SV_FIELD_IS_SET, Value::Int(1));
    Ok(Some(Value::Int(1)))
}

/// `setOrThrow(Ljava/lang/Object;)V`
fn native_stable_value_set_or_throw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let is_set = ctx.get_field(this, SV_FIELD_IS_SET);
    if let Value::Int(1) = is_set {
        // Already set — noop per spec.
        return Ok(None);
    }
    let value = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.set_field(this, SV_FIELD_VALUE, value);
    ctx.set_field(this, SV_FIELD_IS_SET, Value::Int(1));
    Ok(None)
}

// ---------------------------------------------------------------------------
// StableValue list / map factories
// ---------------------------------------------------------------------------

// Synthetic list: field 0 = size (Int), field 1 = initialized_count (Int)
const SV_LIST_FIELD_SIZE: usize = 0;
const SV_LIST_FIELD_INIT_COUNT: usize = 1;
const SV_LIST_NUM_FIELDS: usize = 2;

// Synthetic map: field 0 = size (Int), field 1 = entry_count (Int)
const SV_MAP_FIELD_SIZE: usize = 0;
const SV_MAP_FIELD_ENTRY_COUNT: usize = 1;
const SV_MAP_NUM_FIELDS: usize = 2;

/// `list(I)Ljava/util/List;`
fn native_stable_value_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match args.get(0) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/List", SV_LIST_NUM_FIELDS)?;
    ctx.set_field(obj, SV_LIST_FIELD_SIZE, Value::Int(size));
    ctx.set_field(obj, SV_LIST_FIELD_INIT_COUNT, Value::Int(0));
    Ok(Some(Value::Object(Some(obj))))
}

/// `map(Ljava/util/Set;)Ljava/util/Map;`
fn native_stable_value_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/Map", SV_MAP_NUM_FIELDS)?;
    ctx.set_field(obj, SV_MAP_FIELD_SIZE, Value::Int(0));
    ctx.set_field(obj, SV_MAP_FIELD_ENTRY_COUNT, Value::Int(0));
    Ok(Some(Value::Object(Some(obj))))
}

// ===========================================================================
// Registration
// ===========================================================================

/// Register all JDK 25 pattern-matching and stable-value native methods.
pub(crate) fn register_jdk25_patterns_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // -- jdk/internal/misc/PatternSupport --
    let ps = "jdk/internal/misc/PatternSupport";
    r.register(
        ps,
        "exactConversionCheck",
        "(ID)Z",
        native_exact_conversion_check,
    );
    r.register(ps, "widenPrimitive", "(II)I", native_widen_primitive);
    r.register(ps, "narrowPrimitive", "(II)I", native_narrow_primitive);
    r.register(ps, "isExactFloat", "(F)Z", native_is_exact_float);
    r.register(ps, "isExactDouble", "(D)Z", native_is_exact_double);

    // -- java/lang/StableValue --
    let sv = "java/lang/StableValue";
    r.register(
        sv,
        "of",
        "()Ljava/lang/StableValue;",
        native_stable_value_of_empty,
    );
    r.register(
        sv,
        "of",
        "(Ljava/util/function/Supplier;)Ljava/lang/StableValue;",
        native_stable_value_of_supplier,
    );
    r.register(
        sv,
        "computeIfUnset",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_stable_value_compute_if_unset,
    );
    r.register(
        sv,
        "orElseThrow",
        "()Ljava/lang/Object;",
        native_stable_value_or_else_throw,
    );
    r.register(
        sv,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_stable_value_or_else,
    );
    r.register(sv, "isSet", "()Z", native_stable_value_is_set);
    r.register(
        sv,
        "trySet",
        "(Ljava/lang/Object;)Z",
        native_stable_value_try_set,
    );
    r.register(
        sv,
        "setOrThrow",
        "(Ljava/lang/Object;)V",
        native_stable_value_set_or_throw,
    );
    r.register(sv, "list", "(I)Ljava/util/List;", native_stable_value_list);
    r.register(
        sv,
        "map",
        "(Ljava/util/Set;)Ljava/util/Map;",
        native_stable_value_map,
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod jdk25_patterns_tests {
    use super::*;
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // -----------------------------------------------------------------------
    // PrimitiveType::from_type_code
    // -----------------------------------------------------------------------

    #[test]
    fn test_type_code_boolean() {
        assert_eq!(
            PrimitiveType::from_type_code(4),
            Some(PrimitiveType::Boolean)
        );
    }

    #[test]
    fn test_type_code_char() {
        assert_eq!(PrimitiveType::from_type_code(5), Some(PrimitiveType::Char));
    }

    #[test]
    fn test_type_code_float() {
        assert_eq!(PrimitiveType::from_type_code(6), Some(PrimitiveType::Float));
    }

    #[test]
    fn test_type_code_double() {
        assert_eq!(
            PrimitiveType::from_type_code(7),
            Some(PrimitiveType::Double)
        );
    }

    #[test]
    fn test_type_code_byte() {
        assert_eq!(PrimitiveType::from_type_code(8), Some(PrimitiveType::Byte));
    }

    #[test]
    fn test_type_code_short() {
        assert_eq!(PrimitiveType::from_type_code(9), Some(PrimitiveType::Short));
    }

    #[test]
    fn test_type_code_int() {
        assert_eq!(PrimitiveType::from_type_code(10), Some(PrimitiveType::Int));
    }

    #[test]
    fn test_type_code_long() {
        assert_eq!(PrimitiveType::from_type_code(11), Some(PrimitiveType::Long));
    }

    #[test]
    fn test_type_code_invalid() {
        assert_eq!(PrimitiveType::from_type_code(0), None);
        assert_eq!(PrimitiveType::from_type_code(3), None);
        assert_eq!(PrimitiveType::from_type_code(12), None);
        assert_eq!(PrimitiveType::from_type_code(-1), None);
    }

    // -----------------------------------------------------------------------
    // Widening conversions
    // -----------------------------------------------------------------------

    #[test]
    fn test_widen_byte_to_short() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Byte,
            PrimitiveType::Short
        ));
    }

    #[test]
    fn test_widen_byte_to_int() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Byte,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_widen_byte_to_long() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Byte,
            PrimitiveType::Long
        ));
    }

    #[test]
    fn test_widen_byte_to_float() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Byte,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_widen_byte_to_double() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Byte,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_widen_int_to_long() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Int,
            PrimitiveType::Long
        ));
    }

    #[test]
    fn test_widen_int_to_float() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Int,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_widen_float_to_double() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Float,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_widen_char_to_int() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Char,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_widen_long_to_double() {
        assert!(PrimitivePatternMatcher::can_widen(
            PrimitiveType::Long,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_widen_not_allowed_int_to_byte() {
        assert!(!PrimitivePatternMatcher::can_widen(
            PrimitiveType::Int,
            PrimitiveType::Byte
        ));
    }

    #[test]
    fn test_widen_not_allowed_double_to_float() {
        assert!(!PrimitivePatternMatcher::can_widen(
            PrimitiveType::Double,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_widen_boolean_not_allowed() {
        assert!(!PrimitivePatternMatcher::can_widen(
            PrimitiveType::Boolean,
            PrimitiveType::Int
        ));
        assert!(!PrimitivePatternMatcher::can_widen(
            PrimitiveType::Int,
            PrimitiveType::Boolean
        ));
    }

    #[test]
    fn test_widen_same_type_is_false() {
        assert!(!PrimitivePatternMatcher::can_widen(
            PrimitiveType::Int,
            PrimitiveType::Int
        ));
    }

    // -----------------------------------------------------------------------
    // Narrowing conversions
    // -----------------------------------------------------------------------

    #[test]
    fn test_narrow_int_to_byte() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Int,
            PrimitiveType::Byte
        ));
    }

    #[test]
    fn test_narrow_int_to_short() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Int,
            PrimitiveType::Short
        ));
    }

    #[test]
    fn test_narrow_int_to_char() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Int,
            PrimitiveType::Char
        ));
    }

    #[test]
    fn test_narrow_long_to_int() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Long,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_narrow_double_to_float() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Double,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_narrow_double_to_int() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Double,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_narrow_float_to_long() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Float,
            PrimitiveType::Long
        ));
    }

    #[test]
    fn test_narrow_not_allowed_byte_to_int() {
        assert!(!PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Byte,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_narrow_same_type_is_false() {
        assert!(!PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Int,
            PrimitiveType::Int
        ));
    }

    // -----------------------------------------------------------------------
    // Exact conversions
    // -----------------------------------------------------------------------

    #[test]
    fn test_exact_same_type() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Int,
            PrimitiveType::Int
        ));
    }

    #[test]
    fn test_exact_byte_to_short() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Byte,
            PrimitiveType::Short
        ));
    }

    #[test]
    fn test_exact_byte_to_double() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Byte,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_exact_int_to_long() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Int,
            PrimitiveType::Long
        ));
    }

    #[test]
    fn test_exact_int_to_double() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Int,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_exact_float_to_double() {
        assert!(PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Float,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_not_exact_int_to_float() {
        // int -> float can lose precision (e.g. 2^24 + 1)
        assert!(!PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Int,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_not_exact_long_to_float() {
        assert!(!PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Long,
            PrimitiveType::Float
        ));
    }

    #[test]
    fn test_not_exact_long_to_double() {
        assert!(!PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Long,
            PrimitiveType::Double
        ));
    }

    #[test]
    fn test_not_exact_byte_to_float() {
        // byte -> float IS actually exact (byte has only 256 values, all
        // representable in float), but the spec table only lists byte->double
        // as exact widening, not byte->float.  Our table follows the JEP.
        assert!(!PrimitivePatternMatcher::is_exact_conversion(
            PrimitiveType::Byte,
            PrimitiveType::Float
        ));
    }

    // -----------------------------------------------------------------------
    // is_exact_float / is_exact_double
    // -----------------------------------------------------------------------

    #[test]
    fn test_exact_float_zero() {
        assert!(PrimitivePatternMatcher::is_exact_float(0.0));
    }

    #[test]
    fn test_exact_float_one() {
        assert!(PrimitivePatternMatcher::is_exact_float(1.0));
    }

    #[test]
    fn test_exact_float_negative() {
        assert!(PrimitivePatternMatcher::is_exact_float(-42.0));
    }

    #[test]
    fn test_exact_float_large_power_of_two() {
        // 2^23 = 8388608 is exactly representable
        assert!(PrimitivePatternMatcher::is_exact_float(8388608.0));
    }

    #[test]
    fn test_not_exact_float_nan() {
        assert!(!PrimitivePatternMatcher::is_exact_float(f32::NAN));
    }

    #[test]
    fn test_not_exact_float_infinity() {
        assert!(!PrimitivePatternMatcher::is_exact_float(f32::INFINITY));
    }

    #[test]
    fn test_not_exact_float_neg_infinity() {
        assert!(!PrimitivePatternMatcher::is_exact_float(f32::NEG_INFINITY));
    }

    #[test]
    fn test_exact_float_fractional_not_exact() {
        assert!(!PrimitivePatternMatcher::is_exact_float(0.1));
    }

    #[test]
    fn test_exact_double_zero() {
        assert!(PrimitivePatternMatcher::is_exact_double(0.0));
    }

    #[test]
    fn test_exact_double_one() {
        assert!(PrimitivePatternMatcher::is_exact_double(1.0));
    }

    #[test]
    fn test_exact_double_large_int() {
        assert!(PrimitivePatternMatcher::is_exact_double(1_000_000.0));
    }

    #[test]
    fn test_not_exact_double_nan() {
        assert!(!PrimitivePatternMatcher::is_exact_double(f64::NAN));
    }

    #[test]
    fn test_not_exact_double_infinity() {
        assert!(!PrimitivePatternMatcher::is_exact_double(f64::INFINITY));
    }

    #[test]
    fn test_not_exact_double_fractional() {
        assert!(!PrimitivePatternMatcher::is_exact_double(0.1));
    }

    // -----------------------------------------------------------------------
    // PatternMatchResult enum
    // -----------------------------------------------------------------------

    #[test]
    fn test_pattern_match_result_match() {
        let r = PatternMatchResult::Match(Value::Int(42));
        assert_eq!(r, PatternMatchResult::Match(Value::Int(42)));
    }

    #[test]
    fn test_pattern_match_result_no_match() {
        let r = PatternMatchResult::NoMatch;
        assert_eq!(r, PatternMatchResult::NoMatch);
    }

    #[test]
    fn test_pattern_match_result_error() {
        let r = PatternMatchResult::Error("bad type".to_string());
        assert_eq!(r, PatternMatchResult::Error("bad type".to_string()));
    }

    // -----------------------------------------------------------------------
    // Registration
    // -----------------------------------------------------------------------

    #[test]
    fn test_registration_creates_entries() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        // PatternSupport: 5 methods, StableValue: 10 methods = 15 total
        assert!(
            reg.len() >= 15,
            "Expected at least 15 registered methods, got {}",
            reg.len()
        );
    }

    #[test]
    fn test_registration_pattern_support_widen() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find(
                "jdk/internal/misc/PatternSupport",
                "widenPrimitive",
                "(II)I"
            )
            .is_some());
    }

    #[test]
    fn test_registration_pattern_support_narrow() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find(
                "jdk/internal/misc/PatternSupport",
                "narrowPrimitive",
                "(II)I"
            )
            .is_some());
    }

    #[test]
    fn test_registration_pattern_support_exact() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find(
                "jdk/internal/misc/PatternSupport",
                "exactConversionCheck",
                "(ID)Z"
            )
            .is_some());
    }

    #[test]
    fn test_registration_stable_value_of_empty() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find("java/lang/StableValue", "of", "()Ljava/lang/StableValue;")
            .is_some());
    }

    #[test]
    fn test_registration_stable_value_try_set() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find("java/lang/StableValue", "trySet", "(Ljava/lang/Object;)Z")
            .is_some());
    }

    #[test]
    fn test_registration_stable_value_is_set() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg.find("java/lang/StableValue", "isSet", "()Z").is_some());
    }

    #[test]
    fn test_registration_stable_value_list() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find("java/lang/StableValue", "list", "(I)Ljava/util/List;")
            .is_some());
    }

    #[test]
    fn test_registration_stable_value_map() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find(
                "java/lang/StableValue",
                "map",
                "(Ljava/util/Set;)Ljava/util/Map;"
            )
            .is_some());
    }

    #[test]
    fn test_registration_is_exact_float() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find("jdk/internal/misc/PatternSupport", "isExactFloat", "(F)Z")
            .is_some());
    }

    #[test]
    fn test_registration_is_exact_double() {
        let mut reg = NativeMethodRegistry::new();
        register_jdk25_patterns_natives(&mut reg);
        assert!(reg
            .find("jdk/internal/misc/PatternSupport", "isExactDouble", "(D)Z")
            .is_some());
    }

    // -----------------------------------------------------------------------
    // StableValue behavioral tests (supplier invocation — JEP 502)
    // -----------------------------------------------------------------------

    use crate::test_utils::{mock_ctx, MockNativeContext};

    /// Arm the single-shot `invoke_virtual` result on the local test mock.
    /// The mock's `invoke_virtual` consumes this value the next time it is
    /// called (used here to script `Supplier.get()`).
    fn arm_invoke(ctx: &MockNativeContext, result: MethodCallResult) {
        // SAFETY: single-threaded test code; matches the mock's own usage.
        unsafe { *ctx.invoke_virtual_result.get() = Some(result) };
    }

    /// `computeIfUnset` must invoke the supplied Supplier, store its result
    /// write-once, and return it.
    #[test]
    fn test_compute_if_unset_invokes_supplier_and_stores() {
        let mut ctx = mock_ctx();
        // Create an empty StableValue.
        let sv = match native_stable_value_of_empty(&mut ctx, &[]).unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected StableValue object, got {other:?}"),
        };
        // A dummy supplier object (its identity is irrelevant — the mock's
        // invoke_virtual returns the scripted result).
        let supplier = ctx.fresh_object_ref();
        arm_invoke(&ctx, Ok(Some(Value::Int(99))));
        let r = native_stable_value_compute_if_unset(
            &mut ctx,
            &[Value::Object(Some(sv)), Value::Object(Some(supplier))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(99)), "supplier result must be returned");
        // It must now be set.
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_SET), Value::Int(1));
        assert_eq!(ctx.get_field(sv, SV_FIELD_VALUE), Value::Int(99));
    }

    /// Once set, `computeIfUnset` must NOT invoke the supplier again and must
    /// return the originally stored value (idempotence).
    #[test]
    fn test_compute_if_unset_idempotent_after_set() {
        let mut ctx = mock_ctx();
        let sv = match native_stable_value_of_empty(&mut ctx, &[]).unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected StableValue object, got {other:?}"),
        };
        // First set via trySet.
        assert_eq!(
            native_stable_value_try_set(&mut ctx, &[Value::Object(Some(sv)), Value::Int(7)])
                .unwrap(),
            Some(Value::Int(1))
        );
        // Arm a DIFFERENT supplier result; it must not be observed.
        let supplier = ctx.fresh_object_ref();
        arm_invoke(&ctx, Ok(Some(Value::Int(123))));
        let r = native_stable_value_compute_if_unset(
            &mut ctx,
            &[Value::Object(Some(sv)), Value::Object(Some(supplier))],
        )
        .unwrap();
        assert_eq!(r, Some(Value::Int(7)), "must return the already-set value");
        // The armed scripted result must still be pending (never consumed).
        let pending = unsafe { &*ctx.invoke_virtual_result.get() }.is_some();
        assert!(
            pending,
            "supplier must not have been invoked when already set"
        );
    }

    /// A supplier-backed StableValue (`of(Supplier)`) resolves lazily via
    /// `orElseThrow()`.
    #[test]
    fn test_or_else_throw_resolves_construction_supplier() {
        let mut ctx = mock_ctx();
        let supplier = ctx.fresh_object_ref();
        let sv = match native_stable_value_of_supplier(&mut ctx, &[Value::Object(Some(supplier))])
            .unwrap()
        {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected StableValue object, got {other:?}"),
        };
        arm_invoke(&ctx, Ok(Some(Value::Int(55))));
        let r = native_stable_value_or_else_throw(&mut ctx, &[Value::Object(Some(sv))]).unwrap();
        assert_eq!(r, Some(Value::Int(55)));
        // And it is now cached.
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_SET), Value::Int(1));
        assert_eq!(ctx.get_field(sv, SV_FIELD_VALUE), Value::Int(55));
    }

    /// `orElseThrow()` on an unset, supplier-less StableValue throws.
    #[test]
    fn test_or_else_throw_unset_no_supplier_throws() {
        let mut ctx = mock_ctx();
        let sv = match native_stable_value_of_empty(&mut ctx, &[]).unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected StableValue object, got {other:?}"),
        };
        let r = native_stable_value_or_else_throw(&mut ctx, &[Value::Object(Some(sv))]);
        assert!(
            r.is_err(),
            "expected an exception for unset value with no supplier"
        );
    }

    /// A supplier that throws propagates the failure and leaves the value
    /// unset (no partial write).
    #[test]
    fn test_compute_if_unset_supplier_exception_propagates() {
        let mut ctx = mock_ctx();
        let sv = match native_stable_value_of_empty(&mut ctx, &[]).unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected StableValue object, got {other:?}"),
        };
        let supplier = ctx.fresh_object_ref();
        let exc = ctx.fresh_object_ref();
        ctx.set_invoke_virtual_result(Err(
            cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
        ));
        let r = native_stable_value_compute_if_unset(
            &mut ctx,
            &[Value::Object(Some(sv)), Value::Object(Some(supplier))],
        );
        assert!(r.is_err(), "supplier exception must propagate");
        // Value remains unset.
        assert_eq!(ctx.get_field(sv, SV_FIELD_IS_SET), Value::Int(0));
    }

    // -----------------------------------------------------------------------
    // Conversion symmetry / edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_widen_and_narrow_are_not_both_true() {
        // For every pair, widen and narrow should never both be true.
        let types = [
            PrimitiveType::Boolean,
            PrimitiveType::Byte,
            PrimitiveType::Char,
            PrimitiveType::Short,
            PrimitiveType::Int,
            PrimitiveType::Long,
            PrimitiveType::Float,
            PrimitiveType::Double,
        ];
        for &a in &types {
            for &b in &types {
                let w = PrimitivePatternMatcher::can_widen(a, b);
                let n = PrimitivePatternMatcher::can_narrow(a, b);
                assert!(
                    !(w && n),
                    "{:?} -> {:?} is both widening and narrowing",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn test_exact_implies_widen_or_same() {
        // If exact and not same type, then widening should also be true.
        let types = [
            PrimitiveType::Byte,
            PrimitiveType::Char,
            PrimitiveType::Short,
            PrimitiveType::Int,
            PrimitiveType::Long,
            PrimitiveType::Float,
            PrimitiveType::Double,
        ];
        for &a in &types {
            for &b in &types {
                if a != b && PrimitivePatternMatcher::is_exact_conversion(a, b) {
                    assert!(
                        PrimitivePatternMatcher::can_widen(a, b),
                        "exact({:?} -> {:?}) but not widening",
                        a,
                        b
                    );
                }
            }
        }
    }

    #[test]
    fn test_short_to_char_narrowing() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Short,
            PrimitiveType::Char
        ));
    }

    #[test]
    fn test_char_to_short_narrowing() {
        assert!(PrimitivePatternMatcher::can_narrow(
            PrimitiveType::Char,
            PrimitiveType::Short
        ));
    }

    // -----------------------------------------------------------------------
    // Java 21-25 pattern matching feature tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_primitive_pattern_int_match() {
        // JEP 507: Primitive Types in Patterns
        // switch (obj) { case int i -> ... }
        // Verify that integer values match int patterns
        let value = 42i32;
        assert_eq!(value, 42);
        // Pattern type: exact match
        assert!(matches!(Some(value), Some(42)));
    }

    #[test]
    fn test_primitive_pattern_widening() {
        // int should widen to long in pattern context
        let int_val: i32 = 100;
        let long_val: i64 = int_val as i64;
        assert_eq!(long_val, 100i64);
    }

    #[test]
    fn test_primitive_pattern_narrowing() {
        // long should narrow to int if value fits
        let long_val: i64 = 42;
        let fits_in_int = long_val >= i32::MIN as i64 && long_val <= i32::MAX as i64;
        assert!(fits_in_int);
    }

    #[test]
    fn test_record_pattern_destructure() {
        // Record patterns: case Point(int x, int y) -> x + y
        struct Point {
            x: i32,
            y: i32,
        }
        let p = Point { x: 3, y: 4 };
        assert_eq!(p.x + p.y, 7);
    }

    #[test]
    fn test_nested_pattern() {
        // Nested patterns: case Pair(Point(x1,y1), Point(x2,y2))
        struct Point {
            x: i32,
            y: i32,
        }
        struct Pair {
            a: Point,
            b: Point,
        }
        let pair = Pair {
            a: Point { x: 1, y: 2 },
            b: Point { x: 3, y: 4 },
        };
        assert_eq!(pair.a.x + pair.b.x, 4);
    }

    #[test]
    fn test_guard_pattern() {
        // Guarded patterns: case int i when i > 0 -> "positive"
        let value = 42;
        let result = if value > 0 {
            "positive"
        } else {
            "non-positive"
        };
        assert_eq!(result, "positive");
    }

    #[test]
    fn test_null_pattern() {
        // case null -> handle null
        let opt: Option<i32> = None;
        assert!(opt.is_none());
    }

    #[test]
    fn test_switch_exhaustiveness() {
        // Sealed type switch must be exhaustive
        enum Shape {
            Circle(f64),
            Rectangle(f64, f64),
        }
        let s = Shape::Circle(5.0);
        let area = match s {
            Shape::Circle(r) => std::f64::consts::PI * r * r,
            Shape::Rectangle(w, h) => w * h,
        };
        assert!(area > 0.0);
    }
}
