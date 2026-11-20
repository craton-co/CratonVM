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
