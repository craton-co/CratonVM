use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Round 13 wave 1 (lane callcost): `Math.fma` is a reviewed `Intrinsic`
/// on `Math` only, and it rounds ONCE -- the two double-rounding traps
/// `R12Rt3FmaEdges` checks, whose unfused or twice-rounded answers are one
/// ulp low.
#[test]
fn math_fma_is_an_intrinsic_on_math_only_and_rounds_once() {
    use cratonvm_native_api::NativeKind;
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let mut strict = NativeMethodRegistry::new();
    register_math_natives(&mut strict, "java/lang/StrictMath");
    assert!(strict
        .find("java/lang/StrictMath", "fma", "(DDD)D")
        .is_none());
    assert!(strict
        .find("java/lang/StrictMath", "fma", "(FFF)F")
        .is_none());
    for desc in ["(DDD)D", "(FFF)F"] {
        assert_eq!(
            math.kind_of("java/lang/Math", "fma", desc),
            Some(NativeKind::Intrinsic),
            "Math.fma{desc}"
        );
    }
    let fma_d = math
        .find("java/lang/Math", "fma", "(DDD)D")
        .expect("Math.fma(DDD)D");
    let fma_f = math
        .find("java/lang/Math", "fma", "(FFF)F")
        .expect("Math.fma(FFF)F");
    let mut ctx = mock_ctx();
    let d = |bits: u64| Value::Double(f64::from_bits(bits));
    // (1+2^-26)(1+2^-27) is a tie; 2^-200 tips it up.
    match fma_d(
        &mut ctx,
        &[
            d(0x3FF0_0000_0400_0000),
            d(0x3FF0_0000_0200_0000),
            d(0x3370_0000_0000_0000),
        ],
    ) {
        Ok(Some(Value::Double(v))) => assert_eq!(v.to_bits(), 0x3FF0_0000_0600_0001),
        other => panic!("Math.fma(DDD)D returned {other:?}"),
    }
    // 0.1 * 10 - 1 is 2^-54 exactly (unfused: 0.0).
    match fma_d(
        &mut ctx,
        &[Value::Double(0.1), Value::Double(10.0), Value::Double(-1.0)],
    ) {
        Ok(Some(Value::Double(v))) => assert_eq!(v.to_bits(), 0x3C90_0000_0000_0000),
        other => panic!("Math.fma(0.1, 10, -1) returned {other:?}"),
    }
    // (1+2^-12)^2 is a float tie; 2^-80 tips it up.
    let h1 = Value::Float(f32::from_bits(0x3F80_0800));
    match fma_f(
        &mut ctx,
        &[h1, h1, Value::Float(f32::from_bits(0x1780_0000))],
    ) {
        Ok(Some(Value::Float(v))) => assert_eq!(v.to_bits(), 0x3F80_1001),
        other => panic!("Math.fma(FFF)F returned {other:?}"),
    }
}


// -----------------------------------------------------------------------
// java.lang.Math backing: which rows are fdlibm, and Math.max/min on NaN.
//
// These assert on the REGISTRY, not on the Rust helpers directly. The bug
// they cover was never a wrong body — `fdlibm::hypot` was correct and
// present the whole time — it was a wrong REGISTRATION, so a test that
// calls the helper it wants would have passed throughout.
// -----------------------------------------------------------------------

/// The operand pair that cost four commons-math tests.
///
/// `GaussNewtonOptimizerWith{Cholesky,LU,QR,SVD}Test.testMaxEvaluations`
/// sets a convergence tolerance of 1e-30 so the optimizer can never
/// converge and must instead exceed its 100-evaluation budget. Its model
/// measures point-to-centre distance with `Vector2D.distance`, i.e.
/// `Math.hypot`. Backed by libm, `Math.hypot` returned the naively-rounded
/// `sqrt(x*x + y*y)` — one ULP below fdlibm here — the fifth residual came
/// out one ULP off, the iteration reached an exact fixed point in nine
/// evaluations, the checker called that convergence, and
/// `TooManyEvaluationsException` was never thrown.
#[test]
fn math_hypot_is_registered_to_fdlibm_not_naive_sqrt() {
    let x = f64::from_bits(0xc049_89b7_291d_9512);
    let y = f64::from_bits(0x4048_6eb2_d16f_96cb);
    // What `sqrt(x*x + y*y)` — and the libm this links — return instead.
    // Asserted so the test names the wrong answer it guards against.
    assert_eq!((x * x + y * y).sqrt().to_bits(), 0x4051_abe8_7778_2c30);
    // Captured from Temurin 25.0.4 `Math.hypot`.
    let hotspot = 0x4051_abe8_7778_2c31_u64;

    let mut ctx = mock_ctx();
    for class in ["java/lang/Math", "java/lang/StrictMath"] {
        let mut registry = NativeMethodRegistry::new();
        register_math_natives(&mut registry, class);
        let cb = registry
            .find(class, "hypot", "(DD)D")
            .expect("hypot must be registered");
        match cb(&mut ctx, &[Value::Double(x), Value::Double(y)]) {
            Ok(Some(Value::Double(v))) => {
                assert_eq!(v.to_bits(), hotspot, "{class}.hypot")
            }
            other => panic!("{class}.hypot returned {other:?}"),
        }
    }
}

/// Every row HotSpot does NOT intrinsify must answer identically from
/// `Math` and `StrictMath`, because in JDK 25 the former is a one-line
/// delegation to the latter and nothing substitutes a different body.
///
/// This is the rule itself, not a sample of its consequences: an edit that
/// re-points one of these at platform libm "because `Math` only owes 1 ULP"
/// fails here rather than in three app suites a week later.
#[test]
fn math_and_strictmath_agree_on_non_intrinsified_rows() {
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let mut strict = NativeMethodRegistry::new();
    register_math_natives(&mut strict, "java/lang/StrictMath");

    let samples = [
        0.1_f64, -0.1, 0.5, -0.75, 0.9999, 1.0, -1.0, 2.5, -3.25, 17.0, 1e-8, 1e8,
    ];
    let mut ctx = mock_ctx();

    let call = |reg: &NativeMethodRegistry,
                ctx: &mut MockNativeContext,
                class: &str,
                name: &str,
                desc: &str,
                args: &[Value]|
     -> u64 {
        let cb = reg
            .find(class, name, desc)
            .unwrap_or_else(|| panic!("{class}.{name} not registered"));
        match cb(ctx, args) {
            Ok(Some(Value::Double(v))) => v.to_bits(),
            other => panic!("{class}.{name} returned {other:?}"),
        }
    };

    for name in ["asin", "acos", "atan", "expm1", "log1p", "sinh", "cosh"] {
        for &x in &samples {
            let args = [Value::Double(x)];
            let m = call(&math, &mut ctx, "java/lang/Math", name, "(D)D", &args);
            let sm = call(
                &strict,
                &mut ctx,
                "java/lang/StrictMath",
                name,
                "(D)D",
                &args,
            );
            assert_eq!(m, sm, "Math.{name}({x}) != StrictMath.{name}({x})");
        }
    }
    for name in ["atan2", "hypot", "IEEEremainder"] {
        for &x in &samples {
            for &y in &samples {
                let args = [Value::Double(x), Value::Double(y)];
                let m = call(&math, &mut ctx, "java/lang/Math", name, "(DD)D", &args);
                let sm = call(
                    &strict,
                    &mut ctx,
                    "java/lang/StrictMath",
                    name,
                    "(DD)D",
                    &args,
                );
                assert_eq!(m, sm, "Math.{name}({x}, {y}) mismatch");
            }
        }
    }
}
/// What `Math.log10` is expected to return: the host libm, PLUS the one
/// correction the registration applies on top of it — a negative argument
/// answers the x86 default QNaN, sign bit SET, which is what HotSpot's
/// `_dlog10` stub yields and what this libm does not. The BACKING is still
/// libm, which is what the ratchet below is checking; without this the row
/// would read that correction as "the split collapsed onto fdlibm".
fn math_log10_expected(x: f64) -> f64 {
    if x < 0.0 {
        f64::from_bits(0xFFF8_0000_0000_0000)
    } else {
        x.log10()
    }
}

/// The other direction of the same rule: the rows HotSpot DOES intrinsify
/// must stay split, `StrictMath` on fdlibm and `Math` on platform libm.
///
/// `math_and_strictmath_agree_on_non_intrinsified_rows` is a one-way
/// ratchet — it fails when a shared row is split, and is blind to a split
/// row being merged. Merging is the cheaper-looking edit ("why do we carry
/// two bodies for `sin`?") and it is the one that costs accuracy, because
/// for every row below fdlibm is measurably FURTHER from HotSpot's answer
/// than the host libm is. Against a HotSpot JDK 25 oracle of 5400 sampled
/// inputs per function (`Math.f` on a stock JVM, so the Intel LIBM
/// intrinsic answer):
///
/// | row     | libm disagrees | fdlibm disagrees |
/// | ---     | ---            | ---              |
/// | `sin`   | 9              | 134              |
/// | `cos`   | 9              | 137              |
/// | `tan`   | 20             | 158              |
/// | `exp`   | 4              | 181              |
/// | `log`   | 0              | 78               |
/// | `log10` | 116            | 125              |
/// | `cbrt`  | 11             | 435              |
/// | `pow`   | 1              | 89               |
///
/// all at 1 ULP. Re-run the same oracle with
/// `-XX:+UnlockDiagnosticVMOptions -XX:DisableIntrinsic=_dsin,_dcos,_dtan,`
/// `_dexp,_dlog,_dlog10,_dpow,_dcbrt,_dtanh` — which makes HotSpot run the
/// Java `StrictMath` bodies — and the right column becomes 0/5400 on every
/// row while the left one becomes the larger. That is the whole story of
/// this split: the port is exact against fdlibm, HotSpot's `Math` is not
/// fdlibm, and libm is the closer of the two approximations to it.
///
/// Asserted against the Rust helpers rather than against pinned result
/// bits, because the `Math` column IS the host's libm and its value is a
/// platform fact — pinning glibc's answer would fail on Windows for a
/// reason that is not a defect.
#[test]
fn math_and_strictmath_stay_split_on_intrinsified_rows() {
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let mut strict = NativeMethodRegistry::new();
    register_math_natives(&mut strict, "java/lang/StrictMath");
    let mut ctx = mock_ctx();

    // `PI` is here so the `log10` row keeps a POSITIVE witness that
    // discriminates: libm answers 0x3FDFD14DB31BA3BA there and fdlibm
    // 0x3FDFD14DB31BA3BB. `-0.1` used to be that witness on its own, and
    // stopped being one when `Math.log10` gained the negative-argument NaN
    // correction below.
    let samples = [
        0.1_f64,
        -0.1,
        0.5,
        0.75,
        0.9999,
        1.0,
        2.5,
        3.25,
        17.0,
        1e-8,
        1e8,
        std::f64::consts::PI,
    ];

    let call1 = |reg: &NativeMethodRegistry,
                 ctx: &mut MockNativeContext,
                 class: &str,
                 name: &str,
                 x: f64|
     -> u64 {
        let cb = reg
            .find(class, name, "(D)D")
            .unwrap_or_else(|| panic!("{class}.{name} not registered"));
        match cb(ctx, &[Value::Double(x)]) {
            Ok(Some(Value::Double(v))) => v.to_bits(),
            other => panic!("{class}.{name} returned {other:?}"),
        }
    };

    // `tanh` is deliberately absent: glibc's `tanh` IS the fdlibm body, so
    // on Linux the two backings coincide bit-for-bit and there is nothing
    // to distinguish. It stays split in `register_math_natives` for the
    // platforms where that coincidence does not hold.
    let unary: [(&str, fn(f64) -> f64, fn(f64) -> f64); 8] = [
        ("sin", |x| x.sin(), cratonvm_types::fdlibm::sin),
        ("cos", |x| x.cos(), cratonvm_types::fdlibm::cos),
        ("tan", |x| x.tan(), cratonvm_types::fdlibm::tan),
        ("exp", |x| x.exp(), cratonvm_types::fdlibm::exp),
        ("log", |x| x.ln(), cratonvm_types::fdlibm::log),
        ("log10", math_log10_expected, cratonvm_types::fdlibm::log10),
        ("cbrt", |x| x.cbrt(), cratonvm_types::fdlibm::cbrt),
        ("tanh", |x| x.tanh(), cratonvm_types::fdlibm::tanh),
    ];
    for (name, libm, fd) in unary {
        for &x in &samples {
            assert_eq!(
                call1(&math, &mut ctx, "java/lang/Math", name, x),
                libm(x).to_bits(),
                "Math.{name}({x}) is not the host libm — the split collapsed"
            );
            assert_eq!(
                call1(&strict, &mut ctx, "java/lang/StrictMath", name, x),
                fd(x).to_bits(),
                "StrictMath.{name}({x}) is not fdlibm — the strict contract broke"
            );
        }
    }

    let math_pow = math.find("java/lang/Math", "pow", "(DD)D").unwrap();
    let strict_pow = strict.find("java/lang/StrictMath", "pow", "(DD)D").unwrap();
    let call2 = |cb: cratonvm_native_api::NativeCallback,
                 ctx: &mut MockNativeContext,
                 x: f64,
                 y: f64|
     -> u64 {
        match cb(ctx, &[Value::Double(x), Value::Double(y)]) {
            Ok(Some(Value::Double(v))) => v.to_bits(),
            other => panic!("pow returned {other:?}"),
        }
    };
    for &x in &samples {
        for &y in &samples {
            // `native_math_pow` keeps four exact integer-exponent shortcuts
            // (`x^0`, `x^1`, `x^2`, `x^-1`); those are not libm calls and
            // are covered by `math_pow_at_integer_exponents_is_within_one_ulp`.
            // `y == 0.5` is HotSpot's `sqrtsd` row (`hotspot_dpow_special_rows`).
            if y == 0.0 || y == 1.0 || y == 2.0 || y == -1.0 || y == 0.5 {
                continue;
            }
            assert_eq!(
                call2(math_pow, &mut ctx, x, y),
                x.powf(y).to_bits(),
                "Math.pow({x}, {y}) is not the host libm — the split collapsed"
            );
            assert_eq!(
                call2(strict_pow, &mut ctx, x, y),
                cratonvm_types::fdlibm::pow(x, y).to_bits(),
                "StrictMath.pow({x}, {y}) is not fdlibm — the strict contract broke"
            );
        }
    }
}

/// ULP distance between two `f64`s, NaNs equal, mixed infinities far apart.
fn ulps_apart(a: f64, b: f64) -> u64 {
    if a == b {
        return 0;
    }
    if a.is_nan() || b.is_nan() {
        return if a.is_nan() && b.is_nan() {
            0
        } else {
            u64::MAX
        };
    }
    if a.is_infinite() || b.is_infinite() {
        return u64::MAX;
    }
    let ord = |x: f64| {
        let bits = x.to_bits() as i64;
        if bits < 0 {
            i64::MIN.wrapping_sub(bits)
        } else {
            bits
        }
    };
    ord(a).wrapping_sub(ord(b)).unsigned_abs()
}

/// `Math.pow` at a whole-number exponent must still be within the 1 ULP
/// `java.lang.Math` promises.
///
/// It was not. `native_math_pow` carried an integer-exponent fast path,
/// `a.powi(b as i32)` for every `|b| < 64`, labelled "HotSpot-style" —
/// HotSpot has no such path. `powi` is repeated squaring and rounds once
/// per multiply, so the error grew with the exponent: 1 ULP at `b = 3`,
/// 9 at 17, **40 at 62**, on 24026 of 35360 sampled inputs. The fast path
/// covered exactly the exponents real numeric code uses —
/// commons-math's `TestFunction.SUM_POW` is `Math.pow(abs(x), i + 2)` and
/// `PERM` is `Math.pow(j + 1, i + 1)` — while the general `Math` census,
/// which draws both operands at random and so essentially never produces a
/// whole-number exponent, reported `pow` as 1 disagreement in 5400 and hid
/// it completely.
///
/// The bound is asserted against `fdlibm::pow` rather than against captured
/// libm bits because `Math.pow` IS the host libm here and its exact value
/// is a platform fact; the ULP *distance* is not.
#[test]
fn math_pow_at_integer_exponents_is_within_one_ulp() {
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let cb = math.find("java/lang/Math", "pow", "(DD)D").unwrap();
    let mut ctx = mock_ctx();
    let mut call = |x: f64, y: f64| -> f64 {
        match cb(&mut ctx, &[Value::Double(x), Value::Double(y)]) {
            Ok(Some(Value::Double(v))) => v,
            other => panic!("Math.pow returned {other:?}"),
        }
    };

    // Captured from Temurin JDK 25 — `Math.pow` and `StrictMath.pow` return
    // the same bits at these three, so they pin a platform-independent fact.
    let base = f64::from_bits(0x3fdf_9400_6b2b_4f00); // 0.49340830293407123
    for (exp, want) in [
        (3.0_f64, 0x3fbe_c041_eae6_1253_u64),
        (24.0, 0x3e67_4582_5a70_10ec),
        (62.0, 0x3bfc_1bcf_c1c7_7a64),
    ] {
        assert_eq!(
            call(base, exp).to_bits(),
            want,
            "Math.pow(0.49340830293407123, {exp})"
        );
    }

    // And the rule itself, across the whole range the fast path used to own.
    let bases = [
        0.5_f64,
        0.9,
        1.5,
        2.0,
        3.7,
        7.25,
        -0.5,
        -1.5,
        -3.7,
        0.49340830293407123,
        1.0000001,
        0.9999999,
    ];
    for &x in &bases {
        for e in -63..=63_i32 {
            let y = f64::from(e);
            let got = call(x, y);
            let want = cratonvm_types::fdlibm::pow(x, y);
            if !got.is_finite() || !want.is_finite() {
                continue; // overflow to infinity is not a rounding question
            }
            let d = ulps_apart(got, want);
            assert!(
                d <= 1,
                "Math.pow({x}, {y}) = {got:e} is {d} ULP from {want:e}; \
                 java.lang.Math promises 1"
            );
        }
    }
}

/// `java.lang.Math.pow` is not C99 `pow` when the base is ±1.
///
/// C returns 1.0 for `pow(1, y)` at every `y` — NaN and infinity included —
/// and `f64::powf` is the C rule. The JLS makes the exponent dominant
/// there: NaN exponent gives NaN, and an infinite exponent on a base of
/// absolute value 1 gives NaN. All five rows below returned `1.0` before
/// this was gated, against HotSpot's NaN.
///
/// The old body's own comment claimed these "fall through to powf,
/// preserving Java/JLS special-value semantics (… `pow(1, ±inf) == NaN` per
/// JLS …)". Falling through to `powf` is precisely what produced the C
/// answer; the comment stated the rule the code did not implement.
#[test]
fn math_pow_follows_the_jls_not_c99_when_the_base_is_one() {
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let cb = math.find("java/lang/Math", "pow", "(DD)D").unwrap();
    let mut ctx = mock_ctx();
    let mut call = |x: f64, y: f64| -> f64 {
        match cb(&mut ctx, &[Value::Double(x), Value::Double(y)]) {
            Ok(Some(Value::Double(v))) => v,
            other => panic!("Math.pow returned {other:?}"),
        }
    };

    for (x, y) in [
        (1.0_f64, f64::NAN),
        (1.0, f64::INFINITY),
        (1.0, f64::NEG_INFINITY),
        (-1.0, f64::INFINITY),
        (-1.0, f64::NEG_INFINITY),
    ] {
        let v = call(x, y);
        assert!(v.is_nan(), "Math.pow({x}, {y}) = {v}, expected NaN");
        // HotSpot hands back the canonical positive NaN here, and so must we
        // — `Double.doubleToRawLongBits` on the result is observable.
        assert_eq!(
            v.to_bits(),
            0x7ff8_0000_0000_0000,
            "Math.pow({x}, {y}) NaN payload"
        );
    }

    // The neighbours that must NOT be swept up: a zero exponent wins over a
    // NaN base, and a NaN base with a non-zero exponent is still NaN.
    assert_eq!(call(f64::NAN, 0.0).to_bits(), 1.0_f64.to_bits());
    assert_eq!(call(f64::NAN, -0.0).to_bits(), 1.0_f64.to_bits());
    assert!(call(f64::NAN, 1.0).is_nan());
    assert_eq!(call(f64::INFINITY, 0.0).to_bits(), 1.0_f64.to_bits());
    assert_eq!(call(1.0, 0.0).to_bits(), 1.0_f64.to_bits());
}

/// `signum` and `ulp` hand back the NaN they were given; `StrictMath.copySign`
/// treats a NaN sign as positive and `Math.copySign` does not.
///
/// All three came out of the second census pass, over the float overloads
/// and the exactly-specified bit-level rows. None is a last-ULP question:
/// each is a straight transcription of `java.lang.Math` that had been
/// written as "produce a NaN" instead of "produce THIS NaN".
#[test]
fn math_signum_ulp_and_copysign_follow_the_jdk_on_nan() {
    let mut math = NativeMethodRegistry::new();
    register_math_natives(&mut math, "java/lang/Math");
    let mut strict = NativeMethodRegistry::new();
    register_math_natives(&mut strict, "java/lang/StrictMath");
    let mut ctx = mock_ctx();

    // A NaN with a payload, and its negative counterpart.
    let nan_f = f32::from_bits(0x7fd2_7d42);
    let neg_nan_f = f32::from_bits(0xffc8_ae0a);
    let nan_d = f64::from_bits(0x7ff9_bd92_d9d4_2c64);

    let f1 = |ctx: &mut MockNativeContext, name: &str, x: f32| -> u32 {
        let cb = math.find("java/lang/Math", name, "(F)F").unwrap();
        match cb(ctx, &[Value::Float(x)]) {
            Ok(Some(Value::Float(v))) => v.to_bits(),
            other => panic!("Math.{name} returned {other:?}"),
        }
    };
    let d1 = |ctx: &mut MockNativeContext, name: &str, x: f64| -> u64 {
        let cb = math.find("java/lang/Math", name, "(D)D").unwrap();
        match cb(ctx, &[Value::Double(x)]) {
            Ok(Some(Value::Double(v))) => v.to_bits(),
            other => panic!("Math.{name} returned {other:?}"),
        }
    };

    // signum returns the argument unchanged, sign bit and payload included.
    assert_eq!(f1(&mut ctx, "signum", nan_f), 0x7fd2_7d42);
    assert_eq!(f1(&mut ctx, "signum", neg_nan_f), 0xffc8_ae0a);
    assert_eq!(d1(&mut ctx, "signum", nan_d), 0x7ff9_bd92_d9d4_2c64);
    // ...and is still signum for ordinary values, including signed zero.
    assert_eq!(f1(&mut ctx, "signum", -7.5), (-1.0_f32).to_bits());
    assert_eq!(d1(&mut ctx, "signum", 7.5), 1.0_f64.to_bits());
    assert_eq!(d1(&mut ctx, "signum", -0.0), (-0.0_f64).to_bits());
    assert_eq!(d1(&mut ctx, "signum", 0.0), 0.0_f64.to_bits());

    // ulp is `Math.abs` on the NaN/infinity arm: payload kept, sign cleared.
    assert_eq!(f1(&mut ctx, "ulp", neg_nan_f), 0x7fc8_ae0a);
    assert_eq!(d1(&mut ctx, "ulp", nan_d), 0x7ff9_bd92_d9d4_2c64);
    assert_eq!(
        d1(&mut ctx, "ulp", f64::NEG_INFINITY),
        f64::INFINITY.to_bits()
    );
    // ...and unchanged for finite values.
    assert_eq!(d1(&mut ctx, "ulp", 1.0), (2.0_f64.powi(-52)).to_bits());

    // copySign: StrictMath reads a NaN sign as +, Math copies its sign bit.
    let magnitude = -3.5_f64;
    let nan_sign = f64::from_bits(0xfff8_0000_0000_0000); // NaN, sign bit SET
    let m = math.find("java/lang/Math", "copySign", "(DD)D").unwrap();
    let sm = strict
        .find("java/lang/StrictMath", "copySign", "(DD)D")
        .unwrap();
    let call = |ctx: &mut MockNativeContext, cb: cratonvm_native_api::NativeCallback| -> f64 {
        match cb(ctx, &[Value::Double(magnitude), Value::Double(nan_sign)]) {
            Ok(Some(Value::Double(v))) => v,
            other => panic!("copySign returned {other:?}"),
        }
    };
    assert_eq!(
        call(&mut ctx, m),
        -3.5,
        "Math.copySign copies the NaN's sign"
    );
    assert_eq!(
        call(&mut ctx, sm),
        3.5,
        "StrictMath.copySign reads NaN as +"
    );
    // A non-NaN sign is treated identically by both.
    for cb in [m, sm] {
        match cb(&mut ctx, &[Value::Double(3.5), Value::Double(-0.0)]) {
            Ok(Some(Value::Double(v))) => assert_eq!(v, -3.5),
            other => panic!("copySign returned {other:?}"),
        }
    }
}

/// The census that found the `hypot` row found this too: `Math.max`/`min`
/// were `f64::max`/`f64::min`, whose IEEE-754-2019 `maxNum` semantics
/// deliberately IGNORE a NaN operand where Java's PROPAGATE it. commons-math's
/// `StatUtilsTest.testMax` expects `NaN` from an array containing one and
/// got `-Infinity` — the fold's identity element — because every step
/// silently dropped the NaN.
#[test]
fn math_max_min_propagate_nan_and_pin_signed_zero() {
    let mut registry = NativeMethodRegistry::new();
    register_math_natives(&mut registry, "java/lang/Math");
    let mut ctx = mock_ctx();

    let d = |ctx: &mut MockNativeContext, name: &str, a: f64, b: f64| -> f64 {
        let cb = registry.find("java/lang/Math", name, "(DD)D").unwrap();
        match cb(ctx, &[Value::Double(a), Value::Double(b)]) {
            Ok(Some(Value::Double(v))) => v,
            other => panic!("Math.{name} returned {other:?}"),
        }
    };
    let f = |ctx: &mut MockNativeContext, name: &str, a: f32, b: f32| -> f32 {
        let cb = registry.find("java/lang/Math", name, "(FF)F").unwrap();
        match cb(ctx, &[Value::Float(a), Value::Float(b)]) {
            Ok(Some(Value::Float(v))) => v,
            other => panic!("Math.{name} returned {other:?}"),
        }
    };

    // NaN wins from either position, in both directions.
    assert!(d(&mut ctx, "max", f64::NAN, f64::NEG_INFINITY).is_nan());
    assert!(d(&mut ctx, "max", f64::NEG_INFINITY, f64::NAN).is_nan());
    assert!(d(&mut ctx, "min", f64::NAN, f64::INFINITY).is_nan());
    assert!(d(&mut ctx, "min", f64::INFINITY, f64::NAN).is_nan());
    assert!(d(&mut ctx, "max", 3.0, f64::NAN).is_nan());
    assert!(d(&mut ctx, "min", 3.0, f64::NAN).is_nan());

    // Signed zero: max prefers +0.0, min prefers -0.0, either order.
    assert_eq!(d(&mut ctx, "max", 0.0, -0.0).to_bits(), 0.0_f64.to_bits());
    assert_eq!(d(&mut ctx, "max", -0.0, 0.0).to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        d(&mut ctx, "min", 0.0, -0.0).to_bits(),
        (-0.0_f64).to_bits()
    );
    assert_eq!(
        d(&mut ctx, "min", -0.0, 0.0).to_bits(),
        (-0.0_f64).to_bits()
    );

    // Ordinary ordering is untouched.
    assert_eq!(d(&mut ctx, "max", 2.0, 7.5), 7.5);
    assert_eq!(d(&mut ctx, "min", 2.0, 7.5), 2.0);

    // Same contract on the float overloads.
    assert!(f(&mut ctx, "max", f32::NAN, f32::NEG_INFINITY).is_nan());
    assert!(f(&mut ctx, "min", 1.0, f32::NAN).is_nan());
    assert_eq!(f(&mut ctx, "max", -0.0, 0.0).to_bits(), 0.0_f32.to_bits());
    assert_eq!(
        f(&mut ctx, "min", 0.0, -0.0).to_bits(),
        (-0.0_f32).to_bits()
    );
}

// -----------------------------------------------------------------------
// Wrapper equals(Object) — the argument's TYPE is part of the contract
// -----------------------------------------------------------------------

fn boxed(
    ctx: &mut crate::test_utils::MockNativeContext,
    cid: u32,
    v: Value,
) -> cratonvm_types::ObjectRef {
    let o = ctx.alloc_object(cratonvm_types::ClassId::new(cid), 1);
    ctx.set_field(o, 0, v);
    o
}

// -----------------------------------------------------------------------
// JLS §5.1.7 boxing caches. Every bound below was measured on Microsoft
// OpenJDK 25.0.3+9 before it was written here; see the comment block above
// `CHARACTER_CACHE` for the transcript.
//
// The caches are process-global and keyed by `vm_identity()`, whose mock
// default is 0 and therefore SHARED by every other test in this suite.
// Each test below claims its own identity so its entries — which dangle
// once its mock heap drops — can never be handed to another test.
// -----------------------------------------------------------------------

fn ref_of(v: Option<Value>) -> cratonvm_types::ObjectRef {
    match v {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a boxed object, got {other:?}"),
    }
}

fn box_char(
    ctx: &mut crate::test_utils::MockNativeContext,
    c: u32,
) -> cratonvm_types::ObjectRef {
    ref_of(native_character_value_of(ctx, &[Value::Int(c as i32)]).unwrap())
}

#[test]
fn character_value_of_is_canonical_through_127_and_fresh_from_128_up() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5101);

    // The row RJdkIntrinsics2 --only=charcls asserts, and the boundary.
    for c in [0u32, 'a' as u32, 126, 127] {
        assert_eq!(
            box_char(&mut ctx, c),
            box_char(&mut ctx, c),
            "Character.valueOf({c}) must be the CANONICAL instance — identity, not equality"
        );
    }

    // The other half of the contract. HotSpot's first non-identical code
    // unit is 128; a cache that "rounds up" to 256 or to the whole BMP
    // fails here, and no equality-shaped assertion would notice.
    for c in [128u32, 255, 0x0400, 0xFFFF] {
        assert_ne!(
            box_char(&mut ctx, c),
            box_char(&mut ctx, c),
            "Character.valueOf({c}) is above the cache and must be a FRESH object"
        );
    }

    // The cached instance still carries its value — a canonical box that
    // returns the wrong char would pass every identity row above.
    let a = box_char(&mut ctx, 'a' as u32);
    assert_eq!(ctx.get_field(a, 0), Value::Int('a' as i32));
}

#[test]
fn byte_value_of_is_canonical_for_all_256_values() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5102);
    for b in -128..=127i32 {
        let x = ref_of(native_byte_value_of(&mut ctx, &[Value::Int(b)]).unwrap());
        let y = ref_of(native_byte_value_of(&mut ctx, &[Value::Int(b)]).unwrap());
        assert_eq!(
            x, y,
            "Byte.valueOf({b}) must be canonical — ByteCache has no uncached arm"
        );
        assert_eq!(ctx.get_field(x, 0), Value::Int(b));
    }
}

#[test]
fn short_value_of_caches_minus_128_to_127_and_nothing_outside_it() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5103);
    for s in [-128i32, -1, 0, 127] {
        let x = ref_of(native_short_value_of(&mut ctx, &[Value::Int(s)]).unwrap());
        let y = ref_of(native_short_value_of(&mut ctx, &[Value::Int(s)]).unwrap());
        assert_eq!(x, y, "Short.valueOf({s}) must be canonical");
    }
    for s in [-32768i32, -129, 128, 32767] {
        let x = ref_of(native_short_value_of(&mut ctx, &[Value::Int(s)]).unwrap());
        let y = ref_of(native_short_value_of(&mut ctx, &[Value::Int(s)]).unwrap());
        assert_ne!(
            x, y,
            "Short.valueOf({s}) is outside the cache and must be fresh"
        );
    }
}

#[test]
fn float_and_double_value_of_must_not_be_canonical() {
    // NEGATIVE CONTROL, and the reason the fix above is three caches and
    // not eight. Float and Double cache NOTHING; measured on HotSpot 25,
    // `Float.valueOf(0f) == Float.valueOf(0f)` is FALSE. Making the family
    // "consistent" here would be a regression.
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5104);
    for f in [0.0f32, 1.0, -1.0] {
        let x = ref_of(native_float_value_of(&mut ctx, &[Value::Float(f)]).unwrap());
        let y = ref_of(native_float_value_of(&mut ctx, &[Value::Float(f)]).unwrap());
        assert_ne!(x, y, "Float.valueOf({f}) must NOT be cached");
    }
    for d in [0.0f64, 1.0, -1.0] {
        let x = ref_of(native_double_value_of(&mut ctx, &[Value::Double(d)]).unwrap());
        let y = ref_of(native_double_value_of(&mut ctx, &[Value::Double(d)]).unwrap());
        assert_ne!(x, y, "Double.valueOf({d}) must NOT be cached");
    }
}

#[test]
fn a_cached_character_is_both_reported_as_a_root_and_remapped_after_a_move() {
    // The pairing test. A cache that is scanned but not remapped survives
    // a non-moving collector and hands out a dangling reference after a
    // compacting one, which is why both hooks are asserted from one body.
    let mut ctx = mock_ctx();
    let vm = 0x5105usize;
    ctx.set_vm_identity(vm);

    let before = box_char(&mut ctx, 'q' as u32);
    let mut roots: Vec<cratonvm_types::ObjectRef> = Vec::new();
    gc_scan_value_of_cache_roots(vm, &mut roots);
    assert!(
        roots.contains(&before),
        "the cached Character was not reported to the GC — it would be swept"
    );

    // A real second allocation stands in for the post-compaction address.
    let moved = ctx.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(before.as_ptr() as usize, moved.as_ptr() as usize);
    gc_update_value_of_cache_refs(vm, &map);

    assert_eq!(
        box_char(&mut ctx, 'q' as u32),
        moved,
        "the cache still points at the pre-move address — remap hook missed CHARACTER_CACHE"
    );
}

// -----------------------------------------------------------------------
// F29 — `IntegerCache.high` is configurable, and `canonical_wrapper_if_cached`
// -----------------------------------------------------------------------

fn box_int(
    ctx: &mut crate::test_utils::MockNativeContext,
    v: i32,
) -> cratonvm_types::ObjectRef {
    ref_of(native_integer_value_of(ctx, &[Value::Int(v)]).unwrap())
}

/// The parse rule, as a PURE function — no mock, no VM, no cache.
///
/// Each row is one of the three independent clauses in `IntegerCache
/// .<clinit>`, and each was MEASURED on OpenJDK 25.0.3+9 (`CacheHigh.java`)
/// before it was written here.
#[test]
fn integer_cache_high_follows_the_jdks_three_clauses() {
    // MEASURED `-D...high=1000`: int.1000 true, int.1001 false.
    assert_eq!(parse_integer_cache_high("1000"), Some(1000));
    // MEASURED `-D...high=50`: int.128 STILL false. `Math.max(v, 127)`
    // means the property can only widen, never narrow. Drop the `.max`
    // and this row is the one that fails.
    assert_eq!(parse_integer_cache_high("50"), Some(127));
    assert_eq!(parse_integer_cache_high("-9"), Some(127));
    // MEASURED `-D...high=abc`: ignored, and the run completes.
    assert_eq!(parse_integer_cache_high("abc"), None);
    assert_eq!(parse_integer_cache_high(""), None);
    // `Integer.parseInt`'s grammar, not `str::parse`'s: a leading `+` is
    // legal, surrounding whitespace is not.
    assert_eq!(parse_integer_cache_high("+300"), Some(300));
    assert_eq!(parse_integer_cache_high(" 300"), None);
    assert_eq!(parse_integer_cache_high("300 "), None);
    // A well-formed value wider than an `int` raises NumberFormatException
    // in the JDK too, and the `catch` swallows it identically.
    assert_eq!(parse_integer_cache_high("99999999999"), None);
    // `Math.min(h, Integer.MAX_VALUE - (-low) - 1)`.
    assert_eq!(
        parse_integer_cache_high(&i32::MAX.to_string()),
        Some(i32::MAX - 129)
    );
}

/// The property must actually reach the cache, and must not drag the other
/// five bounds with it.
#[test]
fn the_integer_cache_widens_on_the_property_and_nothing_else_moves() {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(0x5f29);
    ctx.set_system_property("java.lang.Integer.IntegerCache.high", "1000");

    // MEASURED on HotSpot with the same property: int.128/200/999/1000
    // true, int.1001 false, int.-128 true, int.-129 false.
    assert_eq!(box_int(&mut ctx, 128), box_int(&mut ctx, 128));
    assert_eq!(box_int(&mut ctx, 1000), box_int(&mut ctx, 1000));
    assert_eq!(box_int(&mut ctx, -128), box_int(&mut ctx, -128));
    assert_ne!(box_int(&mut ctx, 1001), box_int(&mut ctx, 1001));
    assert_ne!(box_int(&mut ctx, -129), box_int(&mut ctx, -129));

    // The five that MEASURED `false` at 128 in the very same HotSpot run.
    // This is the mutation guard for a "consistency" edit that routes the
    // bound through the whole family.
    assert_ne!(
        ref_of(native_long_value_of(&mut ctx, &[Value::Long(128)]).unwrap()),
        ref_of(native_long_value_of(&mut ctx, &[Value::Long(128)]).unwrap())
    );
    assert_ne!(
        ref_of(native_short_value_of(&mut ctx, &[Value::Int(128)]).unwrap()),
        ref_of(native_short_value_of(&mut ctx, &[Value::Int(128)]).unwrap())
    );
    assert_ne!(box_char(&mut ctx, 128), box_char(&mut ctx, 128));
}

/// A VM with no property set keeps the JDK default, and the widened VM
/// next door does not leak into it. The caches are process-global; only
/// `vm_identity` separates them.
#[test]
fn the_integer_cache_bound_is_per_vm_not_per_process() {
    let mut wide = mock_ctx();
    wide.set_vm_identity(0x5f2a);
    wide.set_system_property("java.lang.Integer.IntegerCache.high", "500");
    assert_eq!(box_int(&mut wide, 300), box_int(&mut wide, 300));

    let mut plain = mock_ctx();
    plain.set_vm_identity(0x5f2b);
    assert_ne!(
        box_int(&mut plain, 300),
        box_int(&mut plain, 300),
        "a second VM inherited the first VM's bound — the memo is not VM-scoped"
    );
    assert_eq!(box_int(&mut plain, 127), box_int(&mut plain, 127));
}

/// Round 9 wave 10b (wire10b): the JIT helper's peeks answer exactly the
/// object the native would, and only for a slot the native has filled.
#[test]
fn r9w10b_cache_peeks_answer_the_natives_object_and_nothing_else() {
    let mut ctx = mock_ctx();
    let vm = 0x5f3dusize;
    ctx.set_vm_identity(vm);
    // Nothing boxed yet in this VM: every peek is a miss.
    assert_eq!(integer_cache_peek(vm, 7), None);
    assert_eq!(long_cache_peek(vm, 7), None);
    let boxed = box_int(&mut ctx, 7);
    assert_eq!(integer_cache_peek(vm, 7), Some(boxed));
    // A cold in-range slot, and values outside the cache, stay misses.
    assert_eq!(integer_cache_peek(vm, 8), None);
    assert_eq!(integer_cache_peek(vm, 128), None);
    assert_eq!(integer_cache_peek(vm, -129), None);
    assert_eq!(integer_cache_peek(vm, i32::MIN), None);
    let long_boxed = ref_of(native_long_value_of(&mut ctx, &[Value::Long(-5)]).unwrap());
    assert_eq!(long_cache_peek(vm, -5), Some(long_boxed));
    assert_eq!(long_cache_peek(vm, 128), None);
    assert_eq!(long_cache_peek(vm, i64::MIN), None);
    // Another VM never sees this one's boxes.
    assert_eq!(integer_cache_peek(vm + 1, 7), None);
    assert_eq!(long_cache_peek(vm + 1, -5), None);
}

/// JIT round 16 (lane callbind): the raw slot array the optimizing tier's
/// inline cached-range `Integer.valueOf` reads is exactly the table
/// `integer_cache_peek` reads -- `-128..=high` long, slot `v + 128` holding
/// the canonical box's address or `0` -- and it does not move once filed.
#[test]
fn r16_callbind_raw_integer_cache_slots_are_the_peeks_table() {
    let mut ctx = mock_ctx();
    let vm = 0x5f3fusize;
    ctx.set_vm_identity(vm);
    ctx.set_system_property("java.lang.Integer.IntegerCache.high", "300");
    assert_eq!(
        integer_cache_slots_raw(vm),
        None,
        "no table before a valueOf"
    );
    let boxed = box_int(&mut ctx, 250);
    let (base, len) = integer_cache_slots_raw(vm).expect("the valueOf filed the table");
    assert_eq!(len, 300 + 128 + 1);
    assert_eq!(base % 8, 0);
    let word = |v: i32| {
        // SAFETY: `v + 128 < len`, and the table is leaked for the process.
        unsafe { *((base + 8 * (v + 128) as usize) as *const usize) }
    };
    assert_eq!(word(250), boxed.as_ptr() as usize);
    assert_eq!(word(-128), 0, "an unboxed slot reads 0");
    let low = box_int(&mut ctx, -128);
    assert_eq!(word(-128), low.as_ptr() as usize);
    assert_eq!(
        integer_cache_slots_raw(vm),
        Some((base, len)),
        "the table never moves"
    );
    assert_eq!(integer_cache_slots_raw(vm + 1), None);
}

/// JIT round 16 wave 7 (lane callbind, CB16-3): the `Long` table's raw
/// slots are the words `long_cache_peek` reads, 256 of them, and the
/// table does not move once filed.
#[test]
fn r16w7_callbind_raw_long_cache_slots_are_the_peeks_table() {
    let mut ctx = mock_ctx();
    let vm = 0x5f71usize;
    ctx.set_vm_identity(vm);
    assert_eq!(long_cache_slots_raw(vm), None, "no table before a valueOf");
    let boxed = ref_of(native_long_value_of(&mut ctx, &[Value::Long(100)]).unwrap());
    let (base, len) = long_cache_slots_raw(vm).expect("the valueOf filed the table");
    assert_eq!(len, 256);
    assert_eq!(base % 8, 0);
    let word = |v: i64| {
        // SAFETY: `v + 128 < len`, and the table is leaked for the process.
        unsafe { *((base + 8 * (v + 128) as usize) as *const usize) }
    };
    assert_eq!(word(100), boxed.as_ptr() as usize);
    assert_eq!(word(-128), 0, "an unboxed slot reads 0");
    let low = ref_of(native_long_value_of(&mut ctx, &[Value::Long(-128)]).unwrap());
    assert_eq!(word(-128), low.as_ptr() as usize);
    assert_eq!(long_cache_peek(vm, -128), Some(low));
    assert_eq!(
        long_cache_slots_raw(vm),
        Some((base, len)),
        "the table never moves"
    );
    assert_eq!(long_cache_slots_raw(vm + 1), None);
}

/// Round 9 wave 11 (collect11): the lock-free `INTEGER_CACHE_LAST` fast
/// path must never answer one VM's box to another, however the lookups
/// interleave, and every reader (peek, probe, bound memo, native) must
/// agree on the same canonical object.
#[test]
fn r9w11_integer_cache_fast_table_is_vm_scoped_under_interleaving() {
    let mut a = mock_ctx();
    let mut b = mock_ctx();
    let (vm_a, vm_b) = (0x5f41usize, 0x5f42usize);
    a.set_vm_identity(vm_a);
    b.set_vm_identity(vm_b);
    b.set_system_property("java.lang.Integer.IntegerCache.high", "500");
    let a7 = box_int(&mut a, 7);
    let b7 = box_int(&mut b, 7);
    assert_ne!(a7, b7, "two VMs must not share a canonical box");
    for _ in 0..3 {
        // Alternate so the last-table pointer flips on every lookup.
        assert_eq!(integer_cache_peek(vm_a, 7), Some(a7));
        assert_eq!(integer_cache_peek(vm_b, 7), Some(b7));
        assert_eq!(integer_cache_high_for(vm_a), Some(127));
        assert_eq!(integer_cache_high_for(vm_b), Some(500));
        assert_eq!(
            canonical_wrapper_if_cached(vm_a, "I", Value::Int(7)),
            Some(a7)
        );
        assert_eq!(
            canonical_wrapper_if_cached(vm_b, "I", Value::Int(7)),
            Some(b7)
        );
        assert_eq!(box_int(&mut a, 7), a7);
        assert_eq!(box_int(&mut b, 7), b7);
    }
    // Each VM's own bound: 300 is cached only in B.
    let b300 = box_int(&mut b, 300);
    assert_eq!(box_int(&mut b, 300), b300);
    assert_eq!(integer_cache_peek(vm_b, 300), Some(b300));
    assert_eq!(integer_cache_peek(vm_a, 300), None);
    assert_ne!(box_int(&mut a, 300), box_int(&mut a, 300));
    // An identity with no table is a miss everywhere, not a stale hit.
    assert_eq!(integer_cache_high_for(0x5f43), None);
    assert_eq!(integer_cache_peek(0x5f43, 7), None);
}

/// The CAS publish: the first box installed for a value is canonical and
/// a later loser gets the winner back; out-of-table values never install.
#[test]
fn r9w11_integer_cache_table_install_keeps_the_first_box() {
    let mut ctx = mock_ctx();
    let first = ctx.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let second = ctx.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let table = IntegerCacheTable::try_new(0x5f44, 127).expect("256 slots");
    assert_eq!(table.slots.len(), 256);
    assert_eq!(table.get(5), None);
    assert_eq!(table.install(5, first), first);
    assert_eq!(table.install(5, second), first);
    assert_eq!(table.get(5), Some(first));
    // Bounds: -129 and 128 are outside, `i32::MAX` must not overflow.
    assert_eq!(table.install(128, second), second);
    assert_eq!(table.get(128), None);
    assert_eq!(table.install(-129, second), second);
    assert_eq!(table.get(-129), None);
    assert_eq!(table.get(i32::MAX), None);
    assert_eq!(table.get(i32::MIN), None);
    assert_eq!(table.install(-128, second), second);
    assert_eq!(table.get(-128), Some(second));
}

/// The widened region must be REPORTED and REMAPPED, not just allocated.
/// A bound that grows past a hook that still walks 256 slots is a
/// use-after-move that only a compacting collection reveals.
#[test]
fn the_widened_integer_region_is_both_scanned_and_remapped() {
    let mut ctx = mock_ctx();
    let vm = 0x5f2cusize;
    ctx.set_vm_identity(vm);
    ctx.set_system_property("java.lang.Integer.IntegerCache.high", "1000");

    let before = box_int(&mut ctx, 900);
    let mut roots: Vec<cratonvm_types::ObjectRef> = Vec::new();
    gc_scan_value_of_cache_roots(vm, &mut roots);
    assert!(
        roots.contains(&before),
        "the widened region is not reported as a root — it would be swept"
    );

    let moved = ctx.alloc_object(cratonvm_types::ClassId::new(1), 1);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(before.as_ptr() as usize, moved.as_ptr() as usize);
    gc_update_value_of_cache_refs(vm, &map);
    assert_eq!(
        box_int(&mut ctx, 900),
        moved,
        "the widened region was not remapped — the scan and remap sets disagree"
    );
}

/// `canonical_wrapper_if_cached` reads, and only reads.
#[test]
fn canonical_wrapper_if_cached_never_populates_and_agrees_when_it_hits() {
    let mut ctx = mock_ctx();
    let vm = 0x5f2dusize;
    ctx.set_vm_identity(vm);

    // Cold: nothing has boxed anything in this VM, so every probe misses.
    // A helper that populated on demand would return `Some` here — and
    // would have had to run `<clinit>` to do it.
    assert_eq!(canonical_wrapper_if_cached(vm, "I", Value::Int(7)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "C", Value::Int(97)), None);

    // Warm: the answer is the SAME OBJECT the native itself returns, not
    // a private twin that merely behaves the same.
    let i7 = box_int(&mut ctx, 7);
    assert_eq!(
        canonical_wrapper_if_cached(vm, "I", Value::Int(7)),
        Some(i7)
    );
    let ca = box_char(&mut ctx, 97);
    assert_eq!(
        canonical_wrapper_if_cached(vm, "C", Value::Int(97)),
        Some(ca)
    );
    let j5 = ref_of(native_long_value_of(&mut ctx, &[Value::Long(5)]).unwrap());
    assert_eq!(
        canonical_wrapper_if_cached(vm, "J", Value::Long(5)),
        Some(j5)
    );
    let b3 = ref_of(native_byte_value_of(&mut ctx, &[Value::Int(3)]).unwrap());
    assert_eq!(
        canonical_wrapper_if_cached(vm, "B", Value::Int(3)),
        Some(b3)
    );
    let s9 = ref_of(native_short_value_of(&mut ctx, &[Value::Int(9)]).unwrap());
    assert_eq!(
        canonical_wrapper_if_cached(vm, "S", Value::Int(9)),
        Some(s9)
    );

    // The probe itself must not have installed anything: another VM
    // identity still misses for the same values.
    assert_eq!(
        canonical_wrapper_if_cached(0x5f2e, "I", Value::Int(7)),
        None
    );
}

/// The guard that turns an identity fix into a wrong answer if it is
/// dropped: a `long` slot presenting as a compact `Value::Int`.
#[test]
fn canonical_wrapper_if_cached_matches_the_variant_not_only_the_descriptor() {
    let mut ctx = mock_ctx();
    let vm = 0x5f2fusize;
    ctx.set_vm_identity(vm);
    // Populate `Long.valueOf(0)` so the wrong answer is AVAILABLE to be
    // returned. Without this the test passes for the wrong reason.
    let zero = ref_of(native_long_value_of(&mut ctx, &[Value::Long(0)]).unwrap());
    assert_eq!(
        canonical_wrapper_if_cached(vm, "J", Value::Long(0)),
        Some(zero)
    );

    // `("J", Value::Int(5))` must MISS. A descriptor-only match indexes
    // slot 5 + 128 of LONG_CACHE — or worse, defaults the payload to 0 and
    // hands back `zero` for a field holding 5.
    assert_eq!(canonical_wrapper_if_cached(vm, "J", Value::Int(5)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "J", Value::Int(0)), None);
    // The mirror: an `int`-descriptor slot carrying a `Long`.
    let _ = box_int(&mut ctx, 5);
    assert_eq!(canonical_wrapper_if_cached(vm, "I", Value::Long(5)), None);
}

/// The four descriptors this helper must NOT answer, each for its own
/// measured reason.
#[test]
fn canonical_wrapper_if_cached_declines_z_f_d_and_the_out_of_bound_arms() {
    let mut ctx = mock_ctx();
    let vm = 0x5f30usize;
    ctx.set_vm_identity(vm);
    let _ = native_boolean_value_of(&mut ctx, &[Value::Int(1)]);
    let _ = box_int(&mut ctx, 7);

    // `Z`: the canonical Boolean is the live `Boolean.TRUE` static field,
    // which this signature cannot reach. `vm_exec.rs` resolves it itself.
    assert_eq!(canonical_wrapper_if_cached(vm, "Z", Value::Int(1)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "Z", Value::Int(0)), None);
    // `F`/`D`: HotSpot caches neither. MEASURED `neg.floatValueOf` = false.
    assert_eq!(
        canonical_wrapper_if_cached(vm, "F", Value::Float(0.0)),
        None
    );
    assert_eq!(
        canonical_wrapper_if_cached(vm, "D", Value::Double(0.0)),
        None
    );
    // Out of bound, per type. MEASURED `fieldoob.*` = false throughout.
    assert_eq!(canonical_wrapper_if_cached(vm, "I", Value::Int(-129)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "C", Value::Int(128)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "C", Value::Int(-1)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "S", Value::Int(128)), None);
    assert_eq!(canonical_wrapper_if_cached(vm, "J", Value::Long(128)), None);
    // A reference descriptor and a null are misses, not panics.
    assert_eq!(
        canonical_wrapper_if_cached(vm, "Ljava/lang/Integer;", Value::Int(7)),
        None
    );
    assert_eq!(
        canonical_wrapper_if_cached(vm, "I", Value::Object(None)),
        None
    );
}

/// The configurable bound and the read-only probe must agree: a value
/// inside a WIDENED `IntegerCache` is reachable through the probe too.
/// This is the row that fails if the probe hard-codes `-128..=127`.
#[test]
fn canonical_wrapper_if_cached_follows_the_configured_integer_bound() {
    let mut ctx = mock_ctx();
    let vm = 0x5f31usize;
    ctx.set_vm_identity(vm);
    ctx.set_system_property("java.lang.Integer.IntegerCache.high", "1000");
    let i900 = box_int(&mut ctx, 900);
    assert_eq!(
        canonical_wrapper_if_cached(vm, "I", Value::Int(900)),
        Some(i900)
    );
    // Still bounded: 1001 is outside the configured high and the store is
    // sized to the bound, so this is a miss rather than an index panic.
    assert_eq!(canonical_wrapper_if_cached(vm, "I", Value::Int(1001)), None);
    assert_eq!(
        canonical_wrapper_if_cached(vm, "I", Value::Int(i32::MAX)),
        None
    );
}

#[test]
fn wrapper_int_equals_rejects_a_different_class_with_the_same_payload() {
    let mut ctx = mock_ctx();
    // Two distinct classes (e.g. Boolean and Integer) both holding Int(0).
    let a = boxed(&mut ctx, 7, Value::Int(0));
    let b = boxed(&mut ctx, 9, Value::Int(0));
    let same = boxed(&mut ctx, 7, Value::Int(0));
    let other_val = boxed(&mut ctx, 7, Value::Int(1));

    let args = [Value::Object(Some(a)), Value::Object(Some(b))];
    assert_eq!(
        native_wrapper_int_equals(&mut ctx, &args).unwrap(),
        Some(Value::Int(0)),
        "Boolean.FALSE.equals(Integer.valueOf(0)) must be false"
    );

    let args = [Value::Object(Some(a)), Value::Object(Some(same))];
    assert_eq!(
        native_wrapper_int_equals(&mut ctx, &args).unwrap(),
        Some(Value::Int(1))
    );

    let args = [Value::Object(Some(a)), Value::Object(Some(other_val))];
    assert_eq!(
        native_wrapper_int_equals(&mut ctx, &args).unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn wrapper_int_equals_rejects_an_array_argument() {
    // The H2 `TestGetGeneratedKeys` failure: `Boolean.FALSE.equals(new
    // int[] {0})` answered `true` because field 0 of the array decoded as
    // `Int(0)` — the same read that fired the `read_slot` corrupt-cell
    // guard. The class test has to reject it before that read happens.
    let mut ctx = mock_ctx();
    let this = boxed(&mut ctx, 7, Value::Int(0));
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    ctx.set_array_element(arr, 0, Value::Int(0));

    let args = [Value::Object(Some(this)), Value::Object(Some(arr))];
    assert_eq!(
        native_wrapper_int_equals(&mut ctx, &args).unwrap(),
        Some(Value::Int(0))
    );
}

/// Residual of the class-id gate: an ARRAY's header stores its
/// COMPONENT class id, so a `Foo[]` reports the same class id as a plain
/// `Foo` and the id compare alone still lets `fooWrapper.equals(fooArray)`
/// through to the field reads. The primitive-array case above is caught
/// only because a primitive array reports `ClassId(0)`; a REFERENCE array
/// whose component class is the receiver's own class is not. Exercised
/// here with matching ids on both sides, so only the array-kind test can
/// decline it.
#[test]
fn wrapper_int_equals_rejects_a_reference_array_with_the_receivers_class_id() {
    let mut ctx = mock_ctx();
    let this = boxed(&mut ctx, 0, Value::Int(0));
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
    assert_eq!(
        ctx.class_id_of_object(this),
        ctx.class_id_of_object(arr),
        "precondition: the class-id compare alone cannot tell these apart"
    );

    let args = [Value::Object(Some(this)), Value::Object(Some(arr))];
    assert_eq!(
        native_wrapper_int_equals(&mut ctx, &args).unwrap(),
        Some(Value::Int(0)),
        "an array argument must be declined before its body is read as a \
         tagged Value cell"
    );
}

#[test]
fn wrapper_long_equals_also_checks_the_class() {
    let mut ctx = mock_ctx();
    let a = boxed(&mut ctx, 11, Value::Long(1));
    let b = boxed(&mut ctx, 12, Value::Long(1));
    let same = boxed(&mut ctx, 11, Value::Long(1));
    assert_eq!(
        native_wrapper_long_equals(&mut ctx, &[Value::Object(Some(a)), Value::Object(Some(b))])
            .unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        native_wrapper_long_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(same))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn wrapper_float_equals_also_checks_the_class() {
    let mut ctx = mock_ctx();
    let a = boxed(&mut ctx, 13, Value::Float(1.0));
    let b = boxed(&mut ctx, 14, Value::Float(1.0));
    let same = boxed(&mut ctx, 13, Value::Float(1.0));
    assert_eq!(
        native_wrapper_float_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(b))]
        )
        .unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        native_wrapper_float_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(same))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn wrapper_double_equals_also_checks_the_class() {
    let mut ctx = mock_ctx();
    let a = boxed(&mut ctx, 15, Value::Double(1.0));
    let b = boxed(&mut ctx, 16, Value::Double(1.0));
    let same = boxed(&mut ctx, 15, Value::Double(1.0));
    assert_eq!(
        native_wrapper_double_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(b))]
        )
        .unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        native_wrapper_double_equals(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(same))]
        )
        .unwrap(),
        Some(Value::Int(1))
    );
}

// -----------------------------------------------------------------------
// Float/Double parsing
// -----------------------------------------------------------------------

#[test]
fn parse_double_string_accepts_java_float_type_suffixes() {
    assert_eq!(parse_double_string("1d").unwrap(), 1.0);
    assert_eq!(parse_double_string("1D").unwrap(), 1.0);
    assert_eq!(parse_double_string("3.0d").unwrap(), 3.0);
    assert_eq!(parse_double_string("1f").unwrap(), 1.0);
    assert_eq!(parse_double_string("1.25f").unwrap(), 1.25);
    assert_eq!(parse_double_string("10F").unwrap(), 10.0);
    assert_eq!(parse_double_string("9.99F").unwrap(), 9.99);
    assert_eq!(parse_double_string("6.0221415E+23d").unwrap(), 6.0221415E23);
    assert_eq!(parse_double_string("  +1d  ").unwrap(), 1.0);
}

#[test]
fn parse_float_string_accepts_java_float_type_suffixes() {
    assert_eq!(parse_float_string("1d").unwrap(), 1.0);
    assert_eq!(parse_float_string("1D").unwrap(), 1.0);
    assert_eq!(parse_float_string("3.0d").unwrap(), 3.0);
    assert_eq!(parse_float_string("1f").unwrap(), 1.0);
    assert_eq!(parse_float_string("1.25f").unwrap(), 1.25);
    assert_eq!(parse_float_string("10F").unwrap(), 10.0);
    assert_eq!(
        parse_float_string("6.0221415E+23d").unwrap(),
        6.0221415E23_f32
    );
    assert_eq!(parse_float_string("  +1d  ").unwrap(), 1.0);
}

#[test]
fn parse_float_type_suffix_stays_numeric_only() {
    assert!(parse_double_string("NaNd").is_err());
    assert!(parse_double_string("Infinityd").is_err());
    assert!(parse_float_string("-InfinityF").is_err());
    assert!(parse_double_string("1dd").is_err());
    assert!(parse_float_string("1_0d").is_err());
}

// -----------------------------------------------------------------------
// abs
// -----------------------------------------------------------------------

#[test]
fn math_abs_int_positive() {
    let mut ctx = mock_ctx();
    let r = native_math_abs_int(&mut ctx, &[Value::Int(42)]);
    assert_eq!(r.unwrap(), Some(Value::Int(42)));
}

#[test]
fn math_abs_int_negative() {
    let mut ctx = mock_ctx();
    let r = native_math_abs_int(&mut ctx, &[Value::Int(-7)]);
    assert_eq!(r.unwrap(), Some(Value::Int(7)));
}

#[test]
fn math_abs_int_min_wraps() {
    // Java Math.abs(Integer.MIN_VALUE) returns MIN_VALUE (wrapping)
    let mut ctx = mock_ctx();
    let r = native_math_abs_int(&mut ctx, &[Value::Int(i32::MIN)]);
    assert_eq!(r.unwrap(), Some(Value::Int(i32::MIN)));
}

#[test]
fn math_abs_double_negative() {
    let mut ctx = mock_ctx();
    let r = native_math_abs_double(&mut ctx, &[Value::Double(-3.14)]);
    assert_eq!(r.unwrap(), Some(Value::Double(3.14)));
}

#[test]
fn math_abs_float_nan() {
    let mut ctx = mock_ctx();
    let r = native_math_abs_float(&mut ctx, &[Value::Float(f32::NAN)]);
    match r.unwrap() {
        Some(Value::Float(v)) => assert!(v.is_nan()),
        other => panic!("expected NaN Float, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// unsignedMultiplyHigh — high 64 bits of the *unsigned* 128-bit product.
// Hottest leaf in the SunEC P-256 Montgomery field multiply. Reference
// values cross-checked against HotSpot JDK 25 (ecprobe_tmp/UmhVerify); the
// cases below are exactly the ones where the unsigned high product differs
// from the signed `multiplyHigh`.
// -----------------------------------------------------------------------

#[test]
fn math_unsigned_multiply_high() {
    let umh = |a: i64, b: i64| {
        native_math_unsigned_multiply_high(&mut mock_ctx(), &[Value::Long(a), Value::Long(b)])
            .unwrap()
            .unwrap()
    };
    // Small product fits below the high word.
    assert_eq!(umh(2, 3), Value::Long(0));
    // u64::MAX * u64::MAX → high word 0xFFFF_FFFF_FFFF_FFFE.
    assert_eq!(umh(-1, -1), Value::Long(-2));
    // u64::MAX * 2 → high word 1 (signed multiplyHigh would give -1).
    assert_eq!(umh(-1, 2), Value::Long(1));
    // 2^63 * 2^63 = 2^126 → high word 2^62.
    assert_eq!(umh(i64::MIN, i64::MIN), Value::Long(0x4000_0000_0000_0000));
    // 2^63 * 3 → high word 1 (signed would give -2).
    assert_eq!(umh(i64::MIN, 3), Value::Long(1));
    // Arbitrary pair — closed form, with the HotSpot reference value.
    let a = 0xDEAD_BEEF_CAFE_BABEu64;
    let b = 0x0123_4567_89AB_CDEFu64;
    let expected = ((a as u128) * (b as u128) >> 64) as u64 as i64;
    assert_eq!(expected, 0x00fd_5bde_eeb2_a01d);
    assert_eq!(umh(a as i64, b as i64), Value::Long(expected));
    // It must differ from signed multiplyHigh on these high-bit operands.
    let signed = native_math_multiply_high(&mut mock_ctx(), &[Value::Long(-1), Value::Long(2)])
        .unwrap()
        .unwrap();
    assert_eq!(signed, Value::Long(-1));
    assert_ne!(signed, umh(-1, 2));
}

// -----------------------------------------------------------------------
// max / min
// -----------------------------------------------------------------------

#[test]
fn math_max_int_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_max_int(&mut ctx, &[Value::Int(3), Value::Int(7)]);
    assert_eq!(r.unwrap(), Some(Value::Int(7)));
}

#[test]
fn math_min_int_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_min_int(&mut ctx, &[Value::Int(3), Value::Int(7)]);
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

#[test]
fn math_max_double_with_nan() {
    let mut ctx = mock_ctx();
    // The NaN-vs-finite case this comment used to describe — "f64::max(1.0,
    // NaN) returns 1.0 in Rust" — was a correct account of a real
    // divergence from `Math.max`, written next to a call that passes NaN
    // TWICE and so cannot observe it. It cost commons-math's
    // `StatUtilsTest.testMax`. The mixed cases now live in
    // `math_max_min_propagate_nan_and_pin_signed_zero`; this one keeps the
    // degenerate pair.
    let r = native_math_max_double(
        &mut ctx,
        &[Value::Double(f64::NAN), Value::Double(f64::NAN)],
    );
    match r.unwrap() {
        Some(Value::Double(v)) => assert!(v.is_nan()),
        other => panic!("expected NaN Double, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// sqrt, pow, trig
// -----------------------------------------------------------------------

#[test]
fn math_sqrt_positive() {
    let mut ctx = mock_ctx();
    let r = native_math_sqrt(&mut ctx, &[Value::Double(9.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(3.0)));
}

#[test]
fn math_sqrt_negative_is_nan() {
    let mut ctx = mock_ctx();
    let r = native_math_sqrt(&mut ctx, &[Value::Double(-1.0)]);
    match r.unwrap() {
        Some(Value::Double(v)) => assert!(v.is_nan()),
        other => panic!("expected NaN, got {other:?}"),
    }
}

#[test]
fn math_pow_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_pow(&mut ctx, &[Value::Double(2.0), Value::Double(10.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(1024.0)));
}

#[test]
fn math_sin_zero() {
    let mut ctx = mock_ctx();
    let r = native_math_sin(&mut ctx, &[Value::Double(0.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(0.0)));
}

#[test]
fn math_cos_zero() {
    let mut ctx = mock_ctx();
    let r = native_math_cos(&mut ctx, &[Value::Double(0.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(1.0)));
}

// -----------------------------------------------------------------------
// floor, ceil, round
// -----------------------------------------------------------------------

#[test]
fn math_floor_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_floor(&mut ctx, &[Value::Double(2.7)]);
    assert_eq!(r.unwrap(), Some(Value::Double(2.0)));
}

#[test]
fn math_ceil_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_ceil(&mut ctx, &[Value::Double(2.1)]);
    assert_eq!(r.unwrap(), Some(Value::Double(3.0)));
}

#[test]
fn math_round_double_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_round_double(&mut ctx, &[Value::Double(2.5)]);
    assert_eq!(r.unwrap(), Some(Value::Long(3)));
}

#[test]
fn math_round_double_nan_returns_zero() {
    let mut ctx = mock_ctx();
    let r = native_math_round_double(&mut ctx, &[Value::Double(f64::NAN)]);
    assert_eq!(r.unwrap(), Some(Value::Long(0)));
}

#[test]
fn math_round_float_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_round_float(&mut ctx, &[Value::Float(2.5)]);
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

// JDK-6430675: the largest double just below 0.5 must round to 0, NOT 1.
// `0.49999999999999994 + 0.5` rounds up to exactly 1.0 in IEEE-754, so the
// old floor(v + 0.5) form returned 1. The bit-exact algorithm returns 0.
#[test]
fn math_round_double_just_below_half() {
    let mut ctx = mock_ctx();
    // 0.49999999999999994 == nextDown(0.5).
    let just_below = 0.49999999999999994_f64;
    assert!(just_below < 0.5 && just_below + 0.5 == 1.0);
    let r = native_math_round_double(&mut ctx, &[Value::Double(just_below)]);
    assert_eq!(r.unwrap(), Some(Value::Long(0)));
    // Exactly half-way still rounds up (round-half-up).
    let r = native_math_round_double(&mut ctx, &[Value::Double(0.5)]);
    assert_eq!(r.unwrap(), Some(Value::Long(1)));
}

#[test]
fn math_round_double_negatives_and_specials() {
    let mut ctx = mock_ctx();
    let round = |v: f64| match native_math_round_double(&mut mock_ctx(), &[Value::Double(v)])
        .unwrap()
    {
        Some(Value::Long(x)) => x,
        other => panic!("expected Long, got {other:?}"),
    };
    // round(-0.5) == 0 (half-up toward +inf, matching HotSpot).
    assert_eq!(round(-0.5), 0);
    // round(-0.50000000000000011) == -1 (just past -0.5).
    assert_eq!(round(-0.5000000000000001), -1);
    assert_eq!(round(-2.5), -2);
    assert_eq!(round(-2.6), -3);
    assert_eq!(round(2.4), 2);
    // NaN -> 0, infinities clamp to extremes.
    assert_eq!(round(f64::NAN), 0);
    assert_eq!(round(f64::INFINITY), i64::MAX);
    assert_eq!(round(f64::NEG_INFINITY), i64::MIN);
    let _ = &mut ctx;
}

// JDK-6430675 (float variant): the largest float just below 0.5f -> 0.
#[test]
fn math_round_float_just_below_half() {
    let mut ctx = mock_ctx();
    // 0.49999997_f32 == nextDown(0.5f).
    let just_below = 0.49999997_f32;
    assert!(just_below < 0.5_f32 && just_below + 0.5_f32 == 1.0_f32);
    let r = native_math_round_float(&mut ctx, &[Value::Float(just_below)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
    let r = native_math_round_float(&mut ctx, &[Value::Float(0.5_f32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn math_round_float_negatives_and_specials() {
    let round =
        |v: f32| match native_math_round_float(&mut mock_ctx(), &[Value::Float(v)]).unwrap() {
            Some(Value::Int(x)) => x,
            other => panic!("expected Int, got {other:?}"),
        };
    assert_eq!(round(-0.5_f32), 0);
    assert_eq!(round(-2.5_f32), -2);
    assert_eq!(round(-2.6_f32), -3);
    assert_eq!(round(f32::NAN), 0);
    assert_eq!(round(f32::INFINITY), i32::MAX);
    assert_eq!(round(f32::NEG_INFINITY), i32::MIN);
}

// -----------------------------------------------------------------------
// exact arithmetic (overflow detection)
// -----------------------------------------------------------------------

#[test]
fn math_add_exact_int_ok() {
    let mut ctx = mock_ctx();
    let r = native_math_add_exact_int(&mut ctx, &[Value::Int(100), Value::Int(200)]);
    assert_eq!(r.unwrap(), Some(Value::Int(300)));
}

#[test]
fn math_add_exact_int_overflow() {
    let mut ctx = mock_ctx();
    let r = native_math_add_exact_int(&mut ctx, &[Value::Int(i32::MAX), Value::Int(1)]);
    assert!(r.is_err());
}

#[test]
fn math_subtract_exact_int_overflow() {
    let mut ctx = mock_ctx();
    let r = native_math_subtract_exact_int(&mut ctx, &[Value::Int(i32::MIN), Value::Int(1)]);
    assert!(r.is_err());
}

#[test]
fn math_multiply_exact_int_overflow() {
    let mut ctx = mock_ctx();
    let r = native_math_multiply_exact_int(&mut ctx, &[Value::Int(i32::MAX), Value::Int(2)]);
    assert!(r.is_err());
}

#[test]
fn math_negate_exact_int_min_overflows() {
    let mut ctx = mock_ctx();
    let r = native_math_negate_exact_int(&mut ctx, &[Value::Int(i32::MIN)]);
    assert!(r.is_err());
}

// -----------------------------------------------------------------------
// floorDiv, floorMod
// -----------------------------------------------------------------------

#[test]
fn math_floor_div_positive() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_div_int(&mut ctx, &[Value::Int(7), Value::Int(2)]);
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

#[test]
fn math_floor_div_negative_rounds_down() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_div_int(&mut ctx, &[Value::Int(-7), Value::Int(2)]);
    assert_eq!(r.unwrap(), Some(Value::Int(-4))); // Java floorDiv(-7,2) == -4
}

#[test]
fn math_floor_div_by_zero_throws() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_div_int(&mut ctx, &[Value::Int(1), Value::Int(0)]);
    assert!(r.is_err());
}

#[test]
fn math_floor_mod_int_basic() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_mod_int(&mut ctx, &[Value::Int(-7), Value::Int(3)]);
    assert_eq!(r.unwrap(), Some(Value::Int(2))); // Java floorMod(-7,3) == 2
}

// MIN_VALUE / -1 is the one input pair where a checked Rust `/` or `%`
// PANICS rather than returning a value. Java specifies a value for all four
// of these, so a panic here is not just wrong, it aborts the process:
// commons-math's `AccurateMathStrictComparisonTest` reflectively calls
// every `StrictMath` method over edge-case inputs and took the whole VM
// down with it.
#[test]
fn math_floor_div_int_min_by_minus_one_wraps_not_panics() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_div_int(&mut ctx, &[Value::Int(i32::MIN), Value::Int(-1)]);
    assert_eq!(r.unwrap(), Some(Value::Int(i32::MIN)));
}

#[test]
fn math_floor_div_long_min_by_minus_one_wraps_not_panics() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_div_long(&mut ctx, &[Value::Long(i64::MIN), Value::Long(-1)]);
    assert_eq!(r.unwrap(), Some(Value::Long(i64::MIN)));
}

#[test]
fn math_floor_mod_int_min_by_minus_one_is_zero() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_mod_int(&mut ctx, &[Value::Int(i32::MIN), Value::Int(-1)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn math_floor_mod_long_min_by_minus_one_is_zero() {
    let mut ctx = mock_ctx();
    let r = native_math_floor_mod_long(&mut ctx, &[Value::Long(i64::MIN), Value::Long(-1)]);
    assert_eq!(r.unwrap(), Some(Value::Long(0)));
}

#[test]
fn math_floor_div_mod_long_by_zero_throws() {
    let mut ctx = mock_ctx();
    assert!(native_math_floor_div_long(&mut ctx, &[Value::Long(1), Value::Long(0)]).is_err());
    assert!(native_math_floor_mod_long(&mut ctx, &[Value::Long(1), Value::Long(0)]).is_err());
    assert!(native_math_floor_mod_int(&mut ctx, &[Value::Int(1), Value::Int(0)]).is_err());
}

// Cross the four natives against the interpreter's own `idiv`/`irem`
// identity `floorMod(a,b) == a - floorDiv(a,b) * b` over the signed corners,
// so a future edit that swaps a `wrapping_*` back for a checked op fails
// here rather than in a workload.
#[test]
fn math_floor_div_mod_identity_over_signed_corners() {
    let mut ctx = mock_ctx();
    let vals = [i32::MIN, i32::MIN + 1, -7, -1, 0, 1, 7, i32::MAX];
    for &a in &vals {
        for &b in &vals {
            if b == 0 {
                continue;
            }
            let d = match native_math_floor_div_int(&mut ctx, &[Value::Int(a), Value::Int(b)]) {
                Ok(Some(Value::Int(v))) => v,
                other => panic!("floorDiv({a},{b}) -> {other:?}"),
            };
            let m = match native_math_floor_mod_int(&mut ctx, &[Value::Int(a), Value::Int(b)]) {
                Ok(Some(Value::Int(v))) => v,
                other => panic!("floorMod({a},{b}) -> {other:?}"),
            };
            assert_eq!(
                m,
                a.wrapping_sub(d.wrapping_mul(b)),
                "floorMod({a},{b}) must equal a - floorDiv(a,b)*b"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Edge rows pinned against a HotSpot JDK 25 oracle (probes/MathSurfaceSweep)
//
// Every expectation below is a RAW BIT PATTERN recorded from HotSpot, not a
// value reasoned from the javadoc — for NaN the javadoc only says "a NaN",
// and the payload/sign that actually comes back is what callers observe.
// -----------------------------------------------------------------------

/// The negative NaN with a payload that the oracle sweep uses.
const NAN_NEG_PAYLOAD: u64 = 0xFFF8_AE0A_0000_0000;

fn d(v: &MethodCallResult) -> u64 {
    match v {
        Ok(Some(Value::Double(x))) => x.to_bits(),
        other => panic!("expected a double, got {other:?}"),
    }
}
fn f(v: &MethodCallResult) -> u32 {
    match v {
        Ok(Some(Value::Float(x))) => x.to_bits(),
        other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn math_ulp_at_max_value_is_two_to_the_971_not_infinity() {
    let mut ctx = mock_ctx();
    // 0x7CA0000000000000 == 2^971
    for v in [f64::MAX, -f64::MAX] {
        let r = native_math_ulp_double(&mut ctx, &[Value::Double(v)]);
        assert_eq!(d(&r), 0x7CA0_0000_0000_0000, "ulp({v:e})");
    }
    // 0x73800000 == 2^104
    for v in [f32::MAX, -f32::MAX] {
        let r = native_math_ulp_float(&mut ctx, &[Value::Float(v)]);
        assert_eq!(f(&r), 0x7380_0000, "ulpF({v:e})");
    }
}

#[test]
fn math_get_exponent_of_a_subnormal_is_min_exponent_minus_one() {
    let mut ctx = mock_ctx();
    for v in [
        f64::MIN_POSITIVE / 2.0,
        f64::from_bits(1),
        -f64::from_bits(1),
    ] {
        let r = native_math_get_exponent_double(&mut ctx, &[Value::Double(v)]);
        assert_eq!(r.unwrap(), Some(Value::Int(-1023)), "getExponent({v:e})");
    }
    // ...and a normal number still reports its real exponent.
    let r = native_math_get_exponent_double(&mut ctx, &[Value::Double(1.0)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn math_pow_of_unit_base_with_infinite_or_nan_exponent_is_nan() {
    let mut ctx = mock_ctx();
    // An infinite exponent under either unit base: a fresh canonical NaN.
    for base in [1.0f64, -1.0f64] {
        for exp in [f64::INFINITY, f64::NEG_INFINITY] {
            let r = native_math_pow(&mut ctx, &[Value::Double(base), Value::Double(exp)]);
            assert_eq!(d(&r), 0x7FF8_0000_0000_0000, "pow({base}, {exp})");
        }
    }
    // A NaN exponent is where the oracle turns asymmetric: base +1.0 takes
    // the canonical NaN, base -1.0 hands back the NaN OPERAND untouched.
    for exp in [f64::NAN, f64::from_bits(NAN_NEG_PAYLOAD)] {
        let r = native_math_pow(&mut ctx, &[Value::Double(1.0), Value::Double(exp)]);
        assert_eq!(d(&r), 0x7FF8_0000_0000_0000, "pow(1.0, {exp})");
    }
    let r = native_math_pow(
        &mut ctx,
        &[
            Value::Double(-1.0),
            Value::Double(f64::from_bits(NAN_NEG_PAYLOAD)),
        ],
    );
    assert_eq!(d(&r), NAN_NEG_PAYLOAD, "pow(-1.0, NaN) keeps the operand");
    // A unit base with an ordinary exponent is untouched.
    let r = native_math_pow(&mut ctx, &[Value::Double(1.0), Value::Double(3.0)]);
    assert_eq!(d(&r), 1.0f64.to_bits());
}

#[test]
fn math_pow_propagates_a_nan_base_with_its_sign() {
    let mut ctx = mock_ctx();
    let nan = f64::from_bits(NAN_NEG_PAYLOAD);
    for exp in [1.0, -1.0, 3.0, -3.0] {
        let r = native_math_pow(&mut ctx, &[Value::Double(nan), Value::Double(exp)]);
        assert_eq!(d(&r), NAN_NEG_PAYLOAD, "pow(NaN, {exp})");
    }
    // ...but a zero exponent still wins, NaN base or not.
    let r = native_math_pow(&mut ctx, &[Value::Double(nan), Value::Double(0.0)]);
    assert_eq!(d(&r), 1.0f64.to_bits());
    // A NaN exponent under a non-unit base keeps ITS payload.
    let r = native_math_pow(&mut ctx, &[Value::Double(-1.0), Value::Double(nan)]);
    assert_eq!(d(&r), NAN_NEG_PAYLOAD);
}

#[test]
fn math_pow_integer_exponents_outside_the_exact_set_go_through_powf() {
    let mut ctx = mock_ctx();
    // -3 is not one of the four exponents the fast path keeps ({0, 1, 2, -1},
    // the ones that round exactly once), so these go to `powf`. Repeated
    // multiplication plus a reciprocal lands one ulp below the oracle;
    // these two bit patterns are HotSpot JDK 25's answers.
    let r = native_math_pow(&mut ctx, &[Value::Double(0.1), Value::Double(-3.0)]);
    assert_eq!(d(&r), 0x408F_3FFF_FFFF_FFFF);
    let r = native_math_pow(&mut ctx, &[Value::Double(-0.1), Value::Double(-3.0)]);
    assert_eq!(d(&r), 0xC08F_3FFF_FFFF_FFFF);
    // Not asserted here: `pow(4503599627370495.5, -1.0)`. `x^-1` IS in the
    // kept set, so it is a single divide and exact — but the row it lands on
    // still differs from HotSpot's intrinsic by one ulp. That is the
    // documented libm-vs-intrinsic residual, tracked by
    // probes/MathSurfaceSweep rather than pinned in a unit test.
}

#[test]
fn math_log10_of_a_negative_is_the_negative_default_nan() {
    let mut ctx = mock_ctx();
    for v in [-1.0, -1e300, -f64::MIN_POSITIVE, f64::NEG_INFINITY] {
        let r = native_math_log10(&mut ctx, &[Value::Double(v)]);
        assert_eq!(d(&r), 0xFFF8_0000_0000_0000, "log10({v:e})");
    }
    // -0.0 is not negative for this purpose: it is still -Infinity.
    let r = native_math_log10(&mut ctx, &[Value::Double(-0.0)]);
    assert_eq!(d(&r), f64::NEG_INFINITY.to_bits());
}

#[test]
fn math_rint_clears_a_nan_sign_but_keeps_its_payload() {
    let mut ctx = mock_ctx();
    let r = native_math_rint(&mut ctx, &[Value::Double(f64::from_bits(NAN_NEG_PAYLOAD))]);
    assert_eq!(d(&r), 0x7FF8_AE0A_0000_0000);
    // ties-to-even is unchanged for ordinary values.
    let r = native_math_rint(&mut ctx, &[Value::Double(2.5)]);
    assert_eq!(d(&r), 2.0f64.to_bits());
}


#[test]
fn math_next_up_down_and_after_propagate_a_nan_unchanged() {
    let mut ctx = mock_ctx();
    let nan = f64::from_bits(NAN_NEG_PAYLOAD);
    assert_eq!(
        d(&native_math_next_up_double(&mut ctx, &[Value::Double(nan)])),
        NAN_NEG_PAYLOAD
    );
    assert_eq!(
        d(&native_math_next_down_double(
            &mut ctx,
            &[Value::Double(nan)]
        )),
        NAN_NEG_PAYLOAD
    );
    // Whichever side is the NaN, that NaN is what comes back.
    assert_eq!(
        d(&native_math_next_after(
            &mut ctx,
            &[Value::Double(0.0), Value::Double(nan)]
        )),
        NAN_NEG_PAYLOAD
    );
    assert_eq!(
        d(&native_math_next_after(
            &mut ctx,
            &[Value::Double(nan), Value::Double(0.0)]
        )),
        NAN_NEG_PAYLOAD
    );
    // A non-NaN pair still steps one ulp.
    assert_eq!(
        d(&native_math_next_after(
            &mut ctx,
            &[Value::Double(1.0), Value::Double(2.0)]
        )),
        (1.0f64).to_bits() + 1
    );
}

// -----------------------------------------------------------------------
// signum
// -----------------------------------------------------------------------

#[test]
fn math_signum_double_positive() {
    let mut ctx = mock_ctx();
    let r = native_math_signum_double(&mut ctx, &[Value::Double(42.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(1.0)));
}

#[test]
fn math_signum_double_negative() {
    let mut ctx = mock_ctx();
    let r = native_math_signum_double(&mut ctx, &[Value::Double(-42.0)]);
    assert_eq!(r.unwrap(), Some(Value::Double(-1.0)));
}

#[test]
fn math_signum_double_nan() {
    let mut ctx = mock_ctx();
    let r = native_math_signum_double(&mut ctx, &[Value::Double(f64::NAN)]);
    match r.unwrap() {
        Some(Value::Double(v)) => assert!(v.is_nan()),
        other => panic!("expected NaN, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// random
// -----------------------------------------------------------------------

#[test]
fn math_random_in_range() {
    let mut ctx = mock_ctx();
    let r = native_math_random(&mut ctx, &[]);
    match r.unwrap() {
        Some(Value::Double(v)) => {
            assert!(v >= 0.0 && v < 1.0, "random() returned {v}, expected [0,1)");
        }
        other => panic!("expected Double, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// toRadians / toDegrees
// -----------------------------------------------------------------------

#[test]
fn math_to_radians_180() {
    let mut ctx = mock_ctx();
    let r = native_math_to_radians(&mut ctx, &[Value::Double(180.0)]);
    match r.unwrap() {
        Some(Value::Double(v)) => {
            // Exact, not within 1e-10. JDK 25 computes this as
            // `angdeg * DEGREES_TO_RADIANS` against a precomputed literal
            // whose bit pattern (0x3f91df46a2529d39) is identical to Rust's
            // `PI / 180.0`, so `toRadians(180.0)` is exactly `PI` on both —
            // there is a single right answer and a tolerance only hides a
            // future rewiring. (JDK 8 used `angdeg / 180.0 * PI`, a
            // different expression that rounds differently; if this ever
            // fails, check which formula the backing uses before widening
            // anything.) W7-54-strictmath-fdlibm-family-RETIRED-20260917.md.
            assert_eq!(v.to_bits(), std::f64::consts::PI.to_bits());
        }
        other => panic!("expected Double, got {other:?}"),
    }
}

#[test]
fn math_to_degrees_pi() {
    let mut ctx = mock_ctx();
    let r = native_math_to_degrees(&mut ctx, &[Value::Double(std::f64::consts::PI)]);
    match r.unwrap() {
        Some(Value::Double(v)) => {
            // Exact — see `math_to_radians_180`. JDK 25's
            // `RADIANS_TO_DEGREES` literal is bit-identical to Rust's
            // `180.0 / PI`, and `toDegrees(PI)` is exactly 180.0.
            assert_eq!(v.to_bits(), 180.0f64.to_bits());
        }
        other => panic!("expected Double, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Character utilities
// -----------------------------------------------------------------------

#[test]
fn character_is_digit_true() {
    let mut ctx = mock_ctx();
    let r = native_character_is_digit(&mut ctx, &[Value::Int('5' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn character_is_digit_false() {
    let mut ctx = mock_ctx();
    let r = native_character_is_digit(&mut ctx, &[Value::Int('a' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn character_is_letter_true() {
    let mut ctx = mock_ctx();
    let r = native_character_is_letter(&mut ctx, &[Value::Int('Z' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn character_to_upper_case() {
    let mut ctx = mock_ctx();
    let r = native_character_to_upper_case(&mut ctx, &[Value::Int('a' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int('A' as i32)));
}

#[test]
fn character_to_lower_case() {
    let mut ctx = mock_ctx();
    let r = native_character_to_lower_case(&mut ctx, &[Value::Int('A' as i32)]);
    assert_eq!(r.unwrap(), Some(Value::Int('a' as i32)));
}

// -----------------------------------------------------------------------
// Integer bit operations
// -----------------------------------------------------------------------

#[test]
fn integer_nlz_zero() {
    let mut ctx = mock_ctx();
    let r = native_integer_nlz(&mut ctx, &[Value::Int(0)]);
    assert_eq!(r.unwrap(), Some(Value::Int(32)));
}

#[test]
fn integer_nlz_one() {
    let mut ctx = mock_ctx();
    let r = native_integer_nlz(&mut ctx, &[Value::Int(1)]);
    assert_eq!(r.unwrap(), Some(Value::Int(31)));
}

#[test]
fn integer_bit_count() {
    let mut ctx = mock_ctx();
    let r = native_integer_bit_count(&mut ctx, &[Value::Int(0b10110)]);
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

// -----------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------

#[test]
fn register_math_natives_does_not_panic() {
    let mut registry = NativeMethodRegistry::new();
    register_math_natives(&mut registry, "java/lang/Math");
    assert!(registry.len() > 30);
}

#[test]
fn register_wrapper_natives_does_not_panic() {
    let mut registry = NativeMethodRegistry::new();
    register_wrapper_natives(&mut registry);
    assert!(registry.len() > 50);
}



// STR17-4: under `--real-jdk` on a real image the three `String` stream
// producers are left out by default (the JDK bodies run), and
// `CRATONVM_STRICT_STRING_STREAM_BYTECODE=0` puts them back. Under
// `--compatible` they are left out too since round 17 wave 3, and
// `CRATONVM_COMPAT_STRING_STREAM_BYTECODE=0` puts them back.
