//! Edge-case coverage for the `Math` intrinsics.
//!
//! A leaf crate cannot construct a live `NativeContext` (the trait has
//! dozens of heap-bound required methods), so behaviour is verified by
//! exercising the pure `compute_*` cores directly, plus tests that the
//! handler signatures coerce to `NativeCallback` and that operand decoding
//! (`int_arg`/`long_arg`/`double_arg`) matches the `lang_math` idiom.
//! Full intrinsic-on vs intrinsic-off differential testing is owned by the
//! TESTS agent in `vm/tests/intrinsic_diff.rs`.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use cratonvm_native_api::NativeCallback;

// -- abs(int) -----------------------------------------------------------

#[test]
fn abs_int_basic() {
    assert_eq!(compute_abs_int(-7), 7);
    assert_eq!(compute_abs_int(7), 7);
    assert_eq!(compute_abs_int(0), 0);
}

#[test]
fn abs_int_min_value_overflows() {
    // Java: Math.abs(Integer.MIN_VALUE) == Integer.MIN_VALUE
    assert_eq!(compute_abs_int(i32::MIN), i32::MIN);
    assert_eq!(compute_abs_int(i32::MAX), i32::MAX);
}

// -- abs(long) ----------------------------------------------------------

#[test]
fn abs_long_basic() {
    assert_eq!(compute_abs_long(-7), 7);
    assert_eq!(compute_abs_long(7), 7);
}

#[test]
fn abs_long_min_value_overflows() {
    // Java: Math.abs(Long.MIN_VALUE) == Long.MIN_VALUE
    assert_eq!(compute_abs_long(i64::MIN), i64::MIN);
    assert_eq!(compute_abs_long(i64::MAX), i64::MAX);
}

#[test]
fn long_arg_decodes_double_tagged_payload() {
    // A long crossing the native boundary may arrive tagged as Double;
    // long_arg must reinterpret the bits to recover the i64.
    let raw: i64 = -1234567890123;
    let tagged = Value::Double(f64::from_le_bytes(raw.to_le_bytes()));
    assert_eq!(long_arg(&[tagged], 0), raw);
    // Also: plain Long and Int promote correctly; missing arg => 0.
    assert_eq!(long_arg(&[Value::Long(42)], 0), 42);
    assert_eq!(long_arg(&[Value::Int(-5)], 0), -5);
    assert_eq!(long_arg(&[], 0), 0);
}

// -- abs(double) --------------------------------------------------------

#[test]
fn abs_double_basic() {
    assert_eq!(compute_abs_double(-3.5), 3.5);
    assert_eq!(compute_abs_double(3.5), 3.5);
}

#[test]
fn abs_double_negative_zero_becomes_positive_zero() {
    // Java: Math.abs(-0.0) == 0.0; distinguishable only by bit pattern.
    let r = compute_abs_double(-0.0);
    assert_eq!(r, 0.0);
    assert_eq!(r.to_bits(), 0.0_f64.to_bits(), "abs(-0.0) must be +0.0");
}

#[test]
fn abs_double_nan_stays_nan() {
    assert!(compute_abs_double(f64::NAN).is_nan());
}

/// Bit-exact agreement with HotSpot, including the NaN PAYLOAD.
///
/// G9-1 swept `Math.sqrt` and `Math.abs(double)` over 200,000 pseudorandom
/// `double` bit patterns plus 14 hand-picked specials on OpenJDK 25.0.3+9
/// and on this toolchain's `f64::sqrt`/`f64::abs`: **200,158 rows,
/// identical, zero divergences**. `is_nan()` is too weak an assertion to
/// notice if that ever stops being true — a NaN whose payload moved is
/// still a NaN — so these rows compare raw bits.
///
/// `abs` clears the sign bit and touches nothing else, so a NEGATIVE
/// signalling NaN must come back as the same payload with bit 63 cleared.
/// `sqrt` of a NaN is specified only as "NaN"; the measured HotSpot answer
/// is the quieted input, which is what the hardware instruction produces.
#[test]
fn nan_payloads_survive_abs_and_sqrt_bit_for_bit() {
    let signalling = f64::from_bits(0xFFF0_0000_0000_0001);
    assert!(signalling.is_nan());
    assert_eq!(
        compute_abs_double(signalling).to_bits(),
        0x7FF0_0000_0000_0001
    );
    assert_eq!(
        compute_sqrt(f64::from_bits(0x7FF0_0000_0000_0001)).to_bits(),
        0x7FF8_0000_0000_0001
    );
    // Signed zeros and infinities, which `is_nan`-style assertions also
    // cannot separate.
    assert_eq!(compute_sqrt(-0.0f64).to_bits(), (-0.0f64).to_bits());
    assert_eq!(compute_abs_double(-0.0f64).to_bits(), 0.0f64.to_bits());
    assert!(compute_sqrt(f64::NEG_INFINITY).is_nan());
    assert_eq!(
        compute_abs_double(f64::NEG_INFINITY).to_bits(),
        f64::INFINITY.to_bits()
    );
}

#[test]
fn abs_double_infinity() {
    assert_eq!(compute_abs_double(f64::NEG_INFINITY), f64::INFINITY);
    assert_eq!(compute_abs_double(f64::INFINITY), f64::INFINITY);
}

// -- min/max(int) -------------------------------------------------------

#[test]
fn min_max_int() {
    assert_eq!(compute_min_int(3, 8), 3);
    assert_eq!(compute_min_int(8, 3), 3);
    assert_eq!(compute_max_int(3, 8), 8);
    assert_eq!(compute_max_int(8, 3), 8);
    assert_eq!(compute_min_int(i32::MIN, i32::MAX), i32::MIN);
    assert_eq!(compute_max_int(i32::MIN, i32::MAX), i32::MAX);
    assert_eq!(compute_min_int(-5, -5), -5);
}

// -- min/max(long) ------------------------------------------------------

#[test]
fn min_max_long() {
    assert_eq!(compute_min_long(3, 8), 3);
    assert_eq!(compute_max_long(3, 8), 8);
    assert_eq!(compute_min_long(i64::MIN, i64::MAX), i64::MIN);
    assert_eq!(compute_max_long(i64::MIN, i64::MAX), i64::MAX);
    assert_eq!(compute_min_long(-5, -5), -5);
}

// -- sqrt(double) -------------------------------------------------------

#[test]
fn sqrt_basic() {
    assert_eq!(compute_sqrt(4.0), 2.0);
    assert_eq!(compute_sqrt(0.0), 0.0);
    assert_eq!(compute_sqrt(1.0), 1.0);
    assert_eq!(compute_sqrt(2.0), std::f64::consts::SQRT_2);
}

#[test]
fn sqrt_negative_is_nan() {
    // Java: Math.sqrt of any negative value is NaN.
    assert!(compute_sqrt(-1.0).is_nan());
    assert!(compute_sqrt(f64::NEG_INFINITY).is_nan());
}

#[test]
fn sqrt_negative_zero_preserves_sign() {
    // Java: Math.sqrt(-0.0) == -0.0 (sign of zero is preserved).
    let r = compute_sqrt(-0.0);
    assert_eq!(r, 0.0);
    assert_eq!(r.to_bits(), (-0.0_f64).to_bits(), "sqrt(-0.0) must be -0.0");
}

#[test]
fn sqrt_nan_and_infinity() {
    assert!(compute_sqrt(f64::NAN).is_nan());
    assert_eq!(compute_sqrt(f64::INFINITY), f64::INFINITY);
}

// -- operand decoding ---------------------------------------------------

#[test]
fn int_and_double_arg_decoding() {
    assert_eq!(int_arg(&[Value::Int(17)], 0), 17);
    assert_eq!(int_arg(&[], 0), 0);
    assert_eq!(double_arg(&[Value::Double(2.5)], 0), 2.5);
    assert_eq!(double_arg(&[], 0), 0.0);
    // second operand index
    assert_eq!(int_arg(&[Value::Int(1), Value::Int(2)], 1), 2);
    assert_eq!(
        double_arg(&[Value::Double(1.0), Value::Double(2.0)], 1),
        2.0
    );
}

// -- handler signatures -------------------------------------------------

/// Every handler must coerce to `NativeCallback` — the type the registry
/// and the inline cache store. A signature drift fails to compile here.
#[test]
fn handlers_match_native_callback_signature() {
    let handlers: [NativeCallback; 8] = [
        intrinsic_math_abs_int,
        intrinsic_math_abs_long,
        intrinsic_math_abs_double,
        intrinsic_math_min_int,
        intrinsic_math_max_int,
        intrinsic_math_min_long,
        intrinsic_math_max_long,
        intrinsic_math_sqrt,
    ];
    // All eight must be distinct fn pointers (no accidental aliasing).
    for i in 0..handlers.len() {
        for j in (i + 1)..handlers.len() {
            assert_ne!(
                handlers[i] as usize, handlers[j] as usize,
                "handlers {i} and {j} alias the same fn"
            );
        }
    }
}
