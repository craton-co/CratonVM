// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Math, StrictMath, and Number subclass native method implementations.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use crate::lang_string::{
    format_double, format_float, native_string_chars, native_string_code_point_at,
    native_string_code_point_count, native_string_code_points, native_string_format,
    native_string_format_locale, native_string_formatted, native_string_indent,
    native_string_is_blank, native_string_lines, native_string_offset_by_code_points,
    native_string_region_matches, native_string_region_matches_ic, native_string_repeat,
    native_string_transform, native_string_value_of_int, native_string_value_of_long,
    native_string_value_of_object,
};

pub(crate) fn register_math_natives(registry: &mut NativeMethodRegistry, class: &str) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    // LEAF: every body from `abs` through `IEEEremainder` below is arithmetic
    // on the argument `Value`s — none of them touches `ctx` at all (the
    // parameter is `_ctx` throughout), so none can allocate, safepoint,
    // collect, or raise a JNI-pending exception. `random` is included: its
    // state is a thread-local SplitMix64 word.
    //
    // This is exactly the case the funnel was measured to dominate:
    // `probes/NativeShapeProbe.java` had `Math.abs(int)` — a table-listed
    // interpreter intrinsic — at 330 ns from compiled code against 0.8 ns on
    // HotSpot, because "intrinsic" only chose the callback, it did not skip
    // `safe_native_call`. See `NativeMethodRegistry::set_leaf`.
    registry.set_leaf(true);
    registry.register(class, "abs", "(I)I", native_math_abs_int);
    registry.register(class, "abs", "(J)J", native_math_abs_long);
    registry.register(class, "abs", "(F)F", native_math_abs_float);
    registry.register(class, "abs", "(D)D", native_math_abs_double);
    registry.register(class, "max", "(II)I", native_math_max_int);
    registry.register(class, "max", "(JJ)J", native_math_max_long);
    registry.register(class, "max", "(FF)F", native_math_max_float);
    registry.register(class, "max", "(DD)D", native_math_max_double);
    registry.register(class, "min", "(II)I", native_math_min_int);
    registry.register(class, "min", "(JJ)J", native_math_min_long);
    registry.register(class, "min", "(FF)F", native_math_min_float);
    registry.register(class, "min", "(DD)D", native_math_min_double);
    // `sqrt` is shared deliberately: IEEE 754 requires it CORRECTLY ROUNDED, so
    // `FdLibm.Sqrt.compute` and the hardware instruction compute the same
    // function. This is the one row of the transcendental surface where sharing
    // one backing between the two classes is a theorem rather than a bug.
    registry.register(class, "sqrt", "(D)D", native_math_sqrt);

    // --- The fdlibm family: the ONLY correct split between Math and StrictMath.
    //
    // These two classes are not two names for one thing. `Math.f` promises a
    // 1-ULP bound and semi-monotonicity, which the host libm meets, and is free
    // to use a CPU intrinsic. `StrictMath.f` promises the FDLIBM RESULT, bit for
    // bit, on every platform and every VM — that is the whole reason the class
    // exists. Registering one backing for both made the strict class no stricter
    // than the loose one, and it was not a theoretical deviation. Replaying a
    // HotSpot oracle against the libm we link (MSVC's CRT, Windows x86-64),
    // every one of these functions disagreed with fdlibm:
    //
    //   cbrt 30.98%  cosh 28.55%  sinh 28.08%  pow 9.73%  exp 9.62%
    //   log1p 7.58%  log(0,1) 7.37%  expm1 7.12%  tan 3.95%  asin 2.55%
    //   cos 2.43%  sin 2.37%  tanh 2.32%  acos 0.89%  atan2 0.36%
    //   hypot 0.32%  log10 0.26%  atan 0.01%
    //
    // `log` was found first only because it reached users through
    // `java.util.Random.nextGaussian()`, whose multiplier is
    // `StrictMath.sqrt(-2 * StrictMath.log(s) / s)`: every seeded gaussian
    // stream came out one ULP off HotSpot's. It was not special — see
    // W7-44-numberformat-enum-and-double-tostring.md for that finding and
    // W7-54-strictmath-fdlibm-family.md for the rest.
    //
    // WHICH CLASS GETS WHICH BACKING — measured, not reasoned (2026-08-16).
    //
    // The paragraph that used to stand here said `Math` keeps libm across the
    // board because libm already satisfies `Math`'s 1-ULP contract. That is
    // true of the *specification* and false of the *oracle*. In JDK 25 every
    // one of these `Math` bodies is a one-line `return StrictMath.f(a);` — so
    // `Math.f` and `StrictMath.f` are the same function on HotSpot EXCEPT
    // where HotSpot substitutes an intrinsic. Reproducing HotSpot therefore
    // needs a three-way split, not a two-way one.
    //
    // `probes/MathCensus.java` replayed a HotSpot JDK 25 oracle (5400 inputs
    // per function, six generators from [-1,1] through raw bit patterns)
    // against both candidate backings. Columns are "rows where the backing
    // disagrees with HotSpot's `Math.f`", out of 5400:
    //
    //   function   libm   fdlibm        function   libm   fdlibm
    //   asin         52     0  <--      sin           9    134
    //   acos         90     0  <--      cos           9    137
    //   atan         65     0  <--      tan          20    158
    //   atan2       884     0  <--      exp           4    181
    //   hypot       405     0  <--      log           0     78
    //   sinh         77     0  <--      log10       116    125
    //   cosh        128     0  <--      pow           1     89
    //   expm1         1     0  <--      cbrt         11    435
    //   log1p         0     0  <--      tanh        543    543
    //
    // The left column is exactly the set of these functions HotSpot does NOT
    // intrinsify, and for every one of them fdlibm is not merely closer, it is
    // EXACT — so those rows are registered once, shared by both classes. The
    // right column is HotSpot's intrinsic set (`_dsin`, `_dcos`, `_dtan`,
    // `_dexp`, `_dlog`, `_dlog10`, `_dpow`, `_dcbrt`, `_dtanh`): its answers
    // come from Intel LIBM assembly stubs that match NEITHER candidate, and
    // there the host libm is one to two orders of magnitude closer than
    // fdlibm, so those stay split and `Math` keeps libm.
    //
    // The cost of getting this wrong was four real test failures, not a last-
    // ULP curiosity: commons-math's `GaussNewtonOptimizerWith*Test`
    // `testMaxEvaluations` drives an optimizer whose convergence checker is set
    // to a 1e-30 tolerance so that it can NEVER converge and must instead trip
    // the 100-evaluation budget. `CircleVectorial`'s model calls
    // `Vector2D.distance`, i.e. `Math.hypot`. One ULP of difference in one of
    // five residuals moved the iteration onto a trajectory that reached an
    // exact fixed point in nine evaluations, the checker reported convergence,
    // and `TooManyEvaluationsException` was never thrown. See
    // bug-commonsmath-gaussnewton-testmaxevaluations-no-exception-20260816-FIXED,
    // and bug-commonsmath-iterative-numeric-fp-divergence-cluster-20260816-CLOSED
    // for the per-row census that settles which backing each split row takes.
    let strict = class == "java/lang/StrictMath";

    // --- Shared fdlibm rows (left column above): NOT a `Math`-vs-`StrictMath`
    // choice at all, because HotSpot runs the same Java body for both.
    registry.register(class, "asin", "(D)D", native_fdlibm_asin);
    registry.register(class, "acos", "(D)D", native_fdlibm_acos);
    registry.register(class, "atan", "(D)D", native_fdlibm_atan);
    registry.register(class, "atan2", "(DD)D", native_fdlibm_atan2);
    // --- Split rows (right column above): HotSpot intrinsifies these, so
    // neither backing is exact and libm is the closer approximation.
    if strict {
        registry.register(class, "pow", "(DD)D", native_fdlibm_pow);
        registry.register(class, "sin", "(D)D", native_fdlibm_sin);
        registry.register(class, "cos", "(D)D", native_fdlibm_cos);
        registry.register(class, "tan", "(D)D", native_fdlibm_tan);
        registry.register(class, "log", "(D)D", native_fdlibm_log);
        registry.register(class, "log10", "(D)D", native_fdlibm_log10);
        registry.register(class, "exp", "(D)D", native_fdlibm_exp);
    } else {
        registry.register(class, "pow", "(DD)D", native_math_pow);
        registry.register(class, "sin", "(D)D", native_math_sin);
        registry.register(class, "cos", "(D)D", native_math_cos);
        registry.register(class, "tan", "(D)D", native_math_tan);
        registry.register(class, "log", "(D)D", native_math_log);
        registry.register(class, "log10", "(D)D", native_math_log10);
        registry.register(class, "exp", "(D)D", native_math_exp);
    }
    registry.register(class, "floor", "(D)D", native_math_floor);
    registry.register(class, "ceil", "(D)D", native_math_ceil);
    registry.register(class, "rint", "(D)D", native_math_rint);
    registry.register(class, "round", "(D)J", native_math_round_double);
    registry.register(class, "round", "(F)I", native_math_round_float);
    registry.register(class, "toRadians", "(D)D", native_math_to_radians);
    registry.register(class, "toDegrees", "(D)D", native_math_to_degrees);
    registry.register(class, "random", "()D", native_math_random);
    registry.register(class, "signum", "(D)D", native_math_signum_double);
    registry.register(class, "signum", "(F)F", native_math_signum_float);
    if strict {
        registry.register(class, "cbrt", "(D)D", native_fdlibm_cbrt);
    } else {
        registry.register(class, "cbrt", "(D)D", native_math_cbrt);
    }
    // `IEEEremainder` is shared, and unlike `sqrt` that is not because the old
    // body was already right — it is because there is no latitude here for
    // EITHER class. Both specs say "as prescribed by the IEEE 754 standard",
    // which fixes the result exactly, so `Math.IEEEremainder` is as wrong as
    // `StrictMath.IEEEremainder` when it deviates. The previous body computed
    // `a - (a/b).round() * b`, which is wrong two ways: `round` is ties-AWAY
    // where IEEE 754 requires ties-to-EVEN, and `a/b` overflows to infinity for
    // operands whose remainder is perfectly ordinary. 49.83% of sampled pairs
    // disagreed with fdlibm, with UNBOUNDED error — the worst row in the census
    // by a wide margin, and the only one that was not a last-ULP story.
    registry.register(class, "IEEEremainder", "(DD)D", native_math_ieee_remainder);
    // End of the leaf block. The `*Exact` family below raises
    // `ArithmeticException` on overflow, which is a `MethodCallFailed` return
    // and would qualify — but they are left on the funnel until something
    // measures them, so the leaf list stays "audited", not "assumed".
    registry.set_leaf(false);

    // --- Exact arithmetic (Phase 13 Step 1) ---
    registry.register(class, "addExact", "(II)I", native_math_add_exact_int);
    registry.register(class, "addExact", "(JJ)J", native_math_add_exact_long);
    registry.register(
        class,
        "subtractExact",
        "(II)I",
        native_math_subtract_exact_int,
    );
    registry.register(
        class,
        "subtractExact",
        "(JJ)J",
        native_math_subtract_exact_long,
    );
    registry.register(
        class,
        "multiplyExact",
        "(II)I",
        native_math_multiply_exact_int,
    );
    registry.register(
        class,
        "multiplyExact",
        "(JJ)J",
        native_math_multiply_exact_long,
    );
    registry.register(
        class,
        "incrementExact",
        "(I)I",
        native_math_increment_exact_int,
    );
    registry.register(
        class,
        "incrementExact",
        "(J)J",
        native_math_increment_exact_long,
    );
    registry.register(
        class,
        "decrementExact",
        "(I)I",
        native_math_decrement_exact_int,
    );
    registry.register(
        class,
        "decrementExact",
        "(J)J",
        native_math_decrement_exact_long,
    );
    registry.register(class, "negateExact", "(I)I", native_math_negate_exact_int);
    registry.register(class, "negateExact", "(J)J", native_math_negate_exact_long);

    // --- Floor/ceil division + toIntExact (Phase 13 Step 2) ---
    registry.register(class, "floorDiv", "(II)I", native_math_floor_div_int);
    registry.register(class, "floorDiv", "(JJ)J", native_math_floor_div_long);
    registry.register(class, "floorMod", "(II)I", native_math_floor_mod_int);
    registry.register(class, "floorMod", "(JJ)J", native_math_floor_mod_long);
    registry.register(class, "toIntExact", "(J)I", native_math_to_int_exact);
    registry.register(class, "multiplyHigh", "(JJ)J", native_math_multiply_high);
    registry.register(
        class,
        "unsignedMultiplyHigh",
        "(JJ)J",
        native_math_unsigned_multiply_high,
    );

    // --- Advanced functions (Phase 13 Step 3) ---
    // `hypot`, `log1p`, `expm1`, `sinh` and `cosh` are shared for the reason
    // given above `let strict`: HotSpot has no intrinsic for any of them, so
    // its `Math.f` IS the fdlibm body and a libm backing here is a measured
    // divergence from the oracle (hypot 405/5400, cosh 128, sinh 77). `tanh`
    // stays split: HotSpot's `_dtanh` intrinsic agrees with neither backing.
    registry.register(class, "hypot", "(DD)D", native_fdlibm_hypot);
    registry.register(class, "log1p", "(D)D", native_fdlibm_log1p);
    registry.register(class, "expm1", "(D)D", native_fdlibm_expm1);
    registry.register(class, "sinh", "(D)D", native_fdlibm_sinh);
    registry.register(class, "cosh", "(D)D", native_fdlibm_cosh);
    if strict {
        registry.register(class, "tanh", "(D)D", native_fdlibm_tanh);
    } else {
        registry.register(class, "tanh", "(D)D", native_math_tanh);
    }
    // `copySign` is the one row where `Math` and `StrictMath` differ BY
    // SPECIFICATION rather than by intrinsic: `StrictMath.copySign` is defined
    // as `Math.copySign(magnitude, isNaN(sign) ? 1.0 : sign)` — it treats a NaN
    // sign argument as POSITIVE, where `Math.copySign` copies the NaN's actual
    // sign bit. Registering one body for both made `StrictMath.copySign` return
    // a negative magnitude for 12 of 6000 sampled pairs where HotSpot returns a
    // positive one. Note the direction: here the STRICT class is the looser of
    // the two, which is why sharing looked safe.
    //
    // MEASURED 2026-08-13 on 25.0.3+9-LTS, the two-line demonstration:
    //
    //   StrictMath.copySign(1.0, -NaN) = 3ff0000000000000  (+1.0)
    //   Math.copySign(1.0, -NaN)       = bff0000000000000  (-1.0)
    //
    // `Math`'s answer already matched, so only the strict form was wrong, and
    // only for NaN — `copySign(1.0, -0.0)` is -1.0 in BOTH and must stay.
    if strict {
        registry.register(class, "copySign", "(DD)D", native_strict_copy_sign_double);
        registry.register(class, "copySign", "(FF)F", native_strict_copy_sign_float);
    } else {
        registry.register(class, "copySign", "(DD)D", native_math_copy_sign_double);
        registry.register(class, "copySign", "(FF)F", native_math_copy_sign_float);
    }
    registry.register(class, "nextUp", "(D)D", native_math_next_up_double);
    registry.register(class, "nextDown", "(D)D", native_math_next_down_double);
    registry.register(class, "nextAfter", "(DD)D", native_math_next_after);
    registry.register(class, "ulp", "(D)D", native_math_ulp_double);
    registry.register(class, "ulp", "(F)F", native_math_ulp_float);
    registry.register(
        class,
        "getExponent",
        "(D)I",
        native_math_get_exponent_double,
    );
    registry.set_category(__prev_cat);
}

pub(crate) fn register_wrapper_natives(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Intrinsic);
    // Integer
    registry.register(
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        native_integer_value_of,
    );
    registry.register(
        "java/lang/Integer",
        "intValue",
        "()I",
        native_wrapper_int_value,
    );
    registry.register(
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        native_integer_parse_int,
    );
    registry.register(
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;I)I",
        native_integer_parse_int_radix,
    );
    registry.register(
        "java/lang/Integer",
        "getInteger",
        "(Ljava/lang/String;I)Ljava/lang/Integer;",
        native_integer_get_integer_default,
    );
    registry.register(
        "java/lang/Integer",
        "toString",
        "(I)Ljava/lang/String;",
        native_string_value_of_int,
    );
    registry.register(
        "java/lang/Integer",
        "toHexString",
        "(I)Ljava/lang/String;",
        native_integer_to_hex_string,
    );
    registry.register(
        "java/lang/Integer",
        "numberOfLeadingZeros",
        "(I)I",
        native_integer_nlz,
    );
    registry.register(
        "java/lang/Integer",
        "numberOfTrailingZeros",
        "(I)I",
        native_integer_ntz,
    );
    registry.register(
        "java/lang/Integer",
        "bitCount",
        "(I)I",
        native_integer_bit_count,
    );
    registry.register(
        "java/lang/Integer",
        "reverse",
        "(I)I",
        native_integer_reverse,
    );
    registry.register(
        "java/lang/Integer",
        "reverseBytes",
        "(I)I",
        native_integer_reverse_bytes,
    );
    registry.register(
        "java/lang/Integer",
        "compare",
        "(II)I",
        native_integer_compare,
    );
    registry.register(
        "java/lang/Integer",
        "compareTo",
        "(Ljava/lang/Integer;)I",
        native_integer_compare_to,
    );
    // Long
    registry.register(
        "java/lang/Long",
        "valueOf",
        "(J)Ljava/lang/Long;",
        native_long_value_of,
    );
    registry.register(
        "java/lang/Long",
        "longValue",
        "()J",
        native_wrapper_long_value,
    );
    registry.register(
        "java/lang/Long",
        "parseLong",
        "(Ljava/lang/String;)J",
        native_long_parse_long,
    );
    registry.register(
        "java/lang/Long",
        "getLong",
        "(Ljava/lang/String;J)Ljava/lang/Long;",
        native_long_get_long_default,
    );
    registry.register(
        "java/lang/Long",
        "toString",
        "(J)Ljava/lang/String;",
        native_string_value_of_long,
    );
    registry.register(
        "java/lang/Long",
        "numberOfLeadingZeros",
        "(J)I",
        native_long_nlz,
    );
    registry.register(
        "java/lang/Long",
        "numberOfTrailingZeros",
        "(J)I",
        native_long_ntz,
    );
    registry.register("java/lang/Long", "bitCount", "(J)I", native_long_bit_count);
    registry.register("java/lang/Long", "reverse", "(J)J", native_long_reverse);
    registry.register(
        "java/lang/Long",
        "reverseBytes",
        "(J)J",
        native_long_reverse_bytes,
    );
    registry.register("java/lang/Long", "compare", "(JJ)I", native_long_compare);
    registry.register(
        "java/lang/Long",
        "compareTo",
        "(Ljava/lang/Long;)I",
        native_long_compare_to,
    );
    // Boolean
    registry.register(
        "java/lang/Boolean",
        "valueOf",
        "(Z)Ljava/lang/Boolean;",
        native_boolean_value_of,
    );
    registry.register(
        "java/lang/Boolean",
        "booleanValue",
        "()Z",
        native_wrapper_int_value,
    );
    // Character
    registry.register(
        "java/lang/Character",
        "valueOf",
        "(C)Ljava/lang/Character;",
        native_character_value_of,
    );
    registry.register(
        "java/lang/Character",
        "charValue",
        "()C",
        native_wrapper_int_value,
    );
    registry.register(
        "java/lang/Character",
        "isDigit",
        "(C)Z",
        native_character_is_digit,
    );
    registry.register(
        "java/lang/Character",
        "isDigit",
        "(I)Z",
        native_character_is_digit,
    );
    registry.register(
        "java/lang/Character",
        "isLetter",
        "(C)Z",
        native_character_is_letter,
    );
    registry.register(
        "java/lang/Character",
        "isLetter",
        "(I)Z",
        native_character_is_letter,
    );
    registry.register(
        "java/lang/Character",
        "isWhitespace",
        "(C)Z",
        native_character_is_whitespace,
    );
    registry.register(
        "java/lang/Character",
        "isWhitespace",
        "(I)Z",
        native_character_is_whitespace,
    );
    registry.register(
        "java/lang/Character",
        "isUpperCase",
        "(C)Z",
        native_character_is_upper_case,
    );
    registry.register(
        "java/lang/Character",
        "isUpperCase",
        "(I)Z",
        native_character_is_upper_case,
    );
    registry.register(
        "java/lang/Character",
        "isLowerCase",
        "(C)Z",
        native_character_is_lower_case,
    );
    registry.register(
        "java/lang/Character",
        "isLowerCase",
        "(I)Z",
        native_character_is_lower_case,
    );
    registry.register(
        "java/lang/Character",
        "toUpperCase",
        "(C)C",
        native_character_to_upper_case,
    );
    registry.register(
        "java/lang/Character",
        "toLowerCase",
        "(C)C",
        native_character_to_lower_case,
    );
    // S111r15 — Character.toLowerCase(I)I / toUpperCase(I)I:
    // The (C)C variants delegate to (I)I via JDK bytecode (`iload_0;
    // invokestatic toLowerCase(I)I; i2c; ireturn`). When the JIT compiles
    // the (I)I overload (which itself calls CharacterData.of + virtual
    // toLowerCase), the resulting machine code returns 0 for some
    // inputs, corrupting Spring's `BeanPropertyName.toDashedForm` to
    // produce names like "r\0\0\0\0\0\0\0-\0\0\0\0\0\0" and tripping
    // `InvalidConfigurationPropertyNameException` in SportMe boot. The
    // (C)C variant only triggers because the (I)I bug poisons the
    // CharacterData chain. Register the (I)I forms as natives so the
    // JIT bypass kicks in for the entire chain.
    registry.register(
        "java/lang/Character",
        "toLowerCase",
        "(I)I",
        native_character_to_lower_case_int,
    );
    registry.register(
        "java/lang/Character",
        "toUpperCase",
        "(I)I",
        native_character_to_upper_case_int,
    );
    registry.register(
        "java/lang/Character",
        "isLetterOrDigit",
        "(C)Z",
        native_character_is_letter_or_digit,
    );
    registry.register(
        "java/lang/Character",
        "isLetterOrDigit",
        "(I)Z",
        native_character_is_letter_or_digit,
    );
    // Float
    registry.register(
        "java/lang/Float",
        "valueOf",
        "(F)Ljava/lang/Float;",
        native_float_value_of,
    );
    registry.register(
        "java/lang/Float",
        "floatValue",
        "()F",
        native_wrapper_float_value,
    );
    registry.register(
        "java/lang/Float",
        "intBitsToFloat",
        "(I)F",
        native_float_int_bits_to_float,
    );
    registry.register("java/lang/Float", "isNaN", "(F)Z", native_float_is_nan);
    registry.register(
        "java/lang/Float",
        "isInfinite",
        "(F)Z",
        native_float_is_infinite,
    );
    registry.register(
        "java/lang/Float",
        "parseFloat",
        "(Ljava/lang/String;)F",
        native_float_parse_float,
    );
    registry.register(
        "java/lang/Float",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/Float;",
        native_float_value_of_string,
    );
    registry.register(
        "java/lang/Float",
        "toString",
        "(F)Ljava/lang/String;",
        native_float_to_string,
    );
    registry.register(
        "java/lang/Float",
        "floatToIntBits",
        "(F)I",
        native_float_to_int_bits,
    );
    registry.register("java/lang/Float", "compare", "(FF)I", native_float_compare);
    // Double
    registry.register(
        "java/lang/Double",
        "valueOf",
        "(D)Ljava/lang/Double;",
        native_double_value_of,
    );
    registry.register(
        "java/lang/Double",
        "doubleValue",
        "()D",
        native_wrapper_double_value,
    );
    registry.register("java/lang/Double", "isNaN", "(D)Z", native_double_is_nan);
    registry.register(
        "java/lang/Double",
        "isInfinite",
        "(D)Z",
        native_double_is_infinite,
    );
    registry.register(
        "java/lang/Double",
        "parseDouble",
        "(Ljava/lang/String;)D",
        native_double_parse_double,
    );
    registry.register(
        "java/lang/Double",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/Double;",
        native_double_value_of_string,
    );
    registry.register(
        "java/lang/Double",
        "toString",
        "(D)Ljava/lang/String;",
        native_double_to_string,
    );
    registry.register(
        "java/lang/Double",
        "doubleToLongBits",
        "(D)J",
        native_double_to_long_bits,
    );
    registry.register(
        "java/lang/Double",
        "compare",
        "(DD)I",
        native_double_compare,
    );
    // Byte / Short
    registry.register(
        "java/lang/Byte",
        "valueOf",
        "(B)Ljava/lang/Byte;",
        native_byte_value_of,
    );
    registry.register(
        "java/lang/Byte",
        "byteValue",
        "()B",
        native_wrapper_int_value,
    );
    registry.register(
        "java/lang/Short",
        "valueOf",
        "(S)Ljava/lang/Short;",
        native_short_value_of,
    );
    registry.register(
        "java/lang/Short",
        "shortValue",
        "()S",
        native_wrapper_int_value,
    );
    // Byte.parseByte / Short.parseShort
    registry.register(
        "java/lang/Byte",
        "parseByte",
        "(Ljava/lang/String;)B",
        native_byte_parse_byte,
    );
    registry.register(
        "java/lang/Byte",
        "parseByte",
        "(Ljava/lang/String;I)B",
        native_byte_parse_byte_radix,
    );
    registry.register(
        "java/lang/Short",
        "parseShort",
        "(Ljava/lang/String;)S",
        native_short_parse_short,
    );
    registry.register(
        "java/lang/Short",
        "parseShort",
        "(Ljava/lang/String;I)S",
        native_short_parse_short_radix,
    );
    // Integer.toString(int, int) with radix
    registry.register(
        "java/lang/Integer",
        "toString",
        "(II)Ljava/lang/String;",
        native_integer_to_string_radix,
    );
    // Long.parseLong(String, int) with radix
    registry.register(
        "java/lang/Long",
        "parseLong",
        "(Ljava/lang/String;I)J",
        native_long_parse_long_radix,
    );
    // Long.toString(long, int) with radix
    registry.register(
        "java/lang/Long",
        "toString",
        "(JI)Ljava/lang/String;",
        native_long_to_string_radix,
    );
    // String.format
    registry.register(
        "java/lang/String",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        native_string_format,
    );
    registry.register(
        "java/lang/String",
        "format",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        native_string_format_locale,
    );
    // String.valueOf overloads
    registry.register(
        "java/lang/String",
        "valueOf",
        "(I)Ljava/lang/String;",
        native_string_value_of_int,
    );
    registry.register(
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        native_string_value_of_object,
    );

    // --- String modern methods (Java 11+) ---
    registry.register(
        "java/lang/String",
        "repeat",
        "(I)Ljava/lang/String;",
        native_string_repeat,
    );
    registry.register("java/lang/String", "isBlank", "()Z", native_string_is_blank);
    // strip, stripLeading, stripTrailing already registered above
    registry.register(
        "java/lang/String",
        "chars",
        "()Ljava/util/stream/IntStream;",
        native_string_chars,
    );
    registry.register(
        "java/lang/String",
        "codePoints",
        "()Ljava/util/stream/IntStream;",
        // NOT "same as chars" — that was true only for the BMP, which is what
        // the old comment said and why this went unnoticed. `chars()` yields
        // UTF-16 code UNITS, so a supplementary character arrives as its two
        // surrogates; `codePoints()` must pair them back into one code point.
        // docs/known-issues/jdk-only/W7-95a-string-code-point-family.md
        crate::lang_string::native_string_code_points,
    );
    registry.register(
        "java/lang/String",
        "regionMatches",
        "(ZILjava/lang/String;II)Z",
        native_string_region_matches_ic,
    );
    registry.register(
        "java/lang/String",
        "regionMatches",
        "(ILjava/lang/String;II)Z",
        native_string_region_matches,
    );
    registry.register(
        "java/lang/String",
        "formatted",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        native_string_formatted,
    );
    // Phase 14: String extras
    registry.register(
        "java/lang/String",
        "codePointAt",
        "(I)I",
        native_string_code_point_at,
    );
    registry.register(
        "java/lang/String",
        "codePointCount",
        "(II)I",
        native_string_code_point_count,
    );
    registry.register(
        "java/lang/String",
        "offsetByCodePoints",
        "(II)I",
        native_string_offset_by_code_points,
    );
    registry.register(
        "java/lang/String",
        "lines",
        "()Ljava/util/stream/Stream;",
        native_string_lines,
    );
    registry.register(
        "java/lang/String",
        "indent",
        "(I)Ljava/lang/String;",
        native_string_indent,
    );
    registry.register(
        "java/lang/String",
        "transform",
        "(Ljava/util/function/Function;)Ljava/lang/Object;",
        native_string_transform,
    );

    // -----------------------------------------------------------------------
    // Phase 12: Number wrapper cross-type conversions
    // -----------------------------------------------------------------------

    // Integer cross-type: longValue, floatValue, doubleValue
    registry.register(
        "java/lang/Integer",
        "longValue",
        "()J",
        native_wrapper_int_to_long,
    );
    registry.register(
        "java/lang/Integer",
        "floatValue",
        "()F",
        native_wrapper_int_to_float,
    );
    registry.register(
        "java/lang/Integer",
        "doubleValue",
        "()D",
        native_wrapper_int_to_double,
    );

    // Long cross-type: intValue, floatValue, doubleValue
    registry.register(
        "java/lang/Long",
        "intValue",
        "()I",
        native_wrapper_long_to_int,
    );
    registry.register(
        "java/lang/Long",
        "floatValue",
        "()F",
        native_wrapper_long_to_float,
    );
    registry.register(
        "java/lang/Long",
        "doubleValue",
        "()D",
        native_wrapper_long_to_double,
    );

    // Float cross-type: intValue, longValue, doubleValue
    registry.register(
        "java/lang/Float",
        "intValue",
        "()I",
        native_wrapper_float_to_int,
    );
    registry.register(
        "java/lang/Float",
        "longValue",
        "()J",
        native_wrapper_float_to_long,
    );
    registry.register(
        "java/lang/Float",
        "doubleValue",
        "()D",
        native_wrapper_float_to_double,
    );

    // Double cross-type: intValue, longValue, floatValue
    registry.register(
        "java/lang/Double",
        "intValue",
        "()I",
        native_wrapper_double_to_int,
    );
    registry.register(
        "java/lang/Double",
        "longValue",
        "()J",
        native_wrapper_double_to_long,
    );
    registry.register(
        "java/lang/Double",
        "floatValue",
        "()F",
        native_wrapper_double_to_float,
    );

    // Byte cross-type: intValue, longValue, floatValue, doubleValue
    registry.register(
        "java/lang/Byte",
        "intValue",
        "()I",
        native_wrapper_int_value,
    );
    registry.register(
        "java/lang/Byte",
        "longValue",
        "()J",
        native_wrapper_int_to_long,
    );
    registry.register(
        "java/lang/Byte",
        "floatValue",
        "()F",
        native_wrapper_int_to_float,
    );
    registry.register(
        "java/lang/Byte",
        "doubleValue",
        "()D",
        native_wrapper_int_to_double,
    );

    // Short cross-type: intValue, longValue, floatValue, doubleValue
    registry.register(
        "java/lang/Short",
        "intValue",
        "()I",
        native_wrapper_int_value,
    );
    registry.register(
        "java/lang/Short",
        "longValue",
        "()J",
        native_wrapper_int_to_long,
    );
    registry.register(
        "java/lang/Short",
        "floatValue",
        "()F",
        native_wrapper_int_to_float,
    );
    registry.register(
        "java/lang/Short",
        "doubleValue",
        "()D",
        native_wrapper_int_to_double,
    );

    // -----------------------------------------------------------------------
    // Phase 12: Wrapper instance methods — toString, hashCode, equals
    // -----------------------------------------------------------------------

    // Integer instance methods
    registry.register(
        "java/lang/Integer",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_int_to_string,
    );
    registry.register(
        "java/lang/Integer",
        "hashCode",
        "()I",
        native_wrapper_int_hash_code,
    );
    registry.register(
        "java/lang/Integer",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_int_equals,
    );

    // Long instance methods
    registry.register(
        "java/lang/Long",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_long_to_string_instance,
    );
    registry.register(
        "java/lang/Long",
        "hashCode",
        "()I",
        native_wrapper_long_hash_code,
    );
    registry.register(
        "java/lang/Long",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_long_equals,
    );

    // Float instance methods
    registry.register(
        "java/lang/Float",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_float_to_string_instance,
    );
    registry.register(
        "java/lang/Float",
        "hashCode",
        "()I",
        native_wrapper_float_hash_code,
    );
    registry.register(
        "java/lang/Float",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_float_equals,
    );

    // Double instance methods
    registry.register(
        "java/lang/Double",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_double_to_string_instance,
    );
    registry.register(
        "java/lang/Double",
        "hashCode",
        "()I",
        native_wrapper_double_hash_code,
    );
    registry.register(
        "java/lang/Double",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_double_equals,
    );

    // Boolean instance methods
    registry.register(
        "java/lang/Boolean",
        "toString",
        "()Ljava/lang/String;",
        native_boolean_instance_to_string,
    );
    registry.register(
        "java/lang/Boolean",
        "hashCode",
        "()I",
        native_boolean_hash_code,
    );
    registry.register(
        "java/lang/Boolean",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_int_equals,
    );

    // Character instance methods
    registry.register(
        "java/lang/Character",
        "toString",
        "()Ljava/lang/String;",
        native_character_instance_to_string,
    );
    registry.register(
        "java/lang/Character",
        "hashCode",
        "()I",
        native_wrapper_int_hash_code,
    );
    registry.register(
        "java/lang/Character",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_int_equals,
    );

    // Byte instance methods
    registry.register(
        "java/lang/Byte",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_int_to_string,
    );
    registry.register(
        "java/lang/Byte",
        "hashCode",
        "()I",
        native_wrapper_int_hash_code,
    );
    registry.register(
        "java/lang/Byte",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_int_equals,
    );

    // Short instance methods
    registry.register(
        "java/lang/Short",
        "toString",
        "()Ljava/lang/String;",
        native_wrapper_int_to_string,
    );
    registry.register(
        "java/lang/Short",
        "hashCode",
        "()I",
        native_wrapper_int_hash_code,
    );
    registry.register(
        "java/lang/Short",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_wrapper_int_equals,
    );

    // -----------------------------------------------------------------------
    // Phase 12: Character additional methods
    // -----------------------------------------------------------------------
    registry.register(
        "java/lang/Character",
        "digit",
        "(CI)I",
        native_character_digit,
    );
    registry.register(
        "java/lang/Character",
        "forDigit",
        "(II)C",
        native_character_for_digit,
    );
    registry.register(
        "java/lang/Character",
        "getNumericValue",
        "(C)I",
        native_character_get_numeric_value,
    );
    registry.register(
        "java/lang/Character",
        "charCount",
        "(I)I",
        native_character_char_count,
    );
    registry.register(
        "java/lang/Character",
        "isHighSurrogate",
        "(C)Z",
        native_character_is_high_surrogate,
    );
    registry.register(
        "java/lang/Character",
        "isLowSurrogate",
        "(C)Z",
        native_character_is_low_surrogate,
    );
    registry.register(
        "java/lang/Character",
        "isBmpCodePoint",
        "(I)Z",
        native_character_is_bmp_code_point,
    );
    registry.register(
        "java/lang/Character",
        "isValidCodePoint",
        "(I)Z",
        native_character_is_valid_code_point,
    );
    registry.register(
        "java/lang/Character",
        "isISOControl",
        "(I)Z",
        native_character_is_iso_control,
    );
    registry.register(
        "java/lang/Character",
        "toString",
        "(C)Ljava/lang/String;",
        native_character_static_to_string,
    );
    // Java 21: Character emoji detection methods.
    //
    // W7-95(C1). All five bodies used to be hand-written coarse ranges with
    // comments like "basic emoji ranges" — whole blocks approximated rather
    // than the property enumerated. Measured against HotSpot 25 over every code
    // point `0..=0x10FFFF`, they were wrong on **2,746**:
    //
    //     isEmoji              1282 wrong  (1265 false positives, 17 misses)
    //     isEmojiPresentation  1416 wrong  (1350 false positives, 66 misses)
    //     isEmojiModifierBase    17 wrong  (all misses)
    //     isEmojiComponent       31 wrong  (30 misses, 1 false positive)
    //     isEmojiModifier         0 wrong  <- the one that was a real range
    //
    // The two the census had already caught (`isEmojiPresentation(U+2764)`
    // true-for-false, `isEmojiComponent(U+1F1E6)` false-for-true) were not
    // corner cases: `0x2600..=0x27BF` as "emoji presentation" claims 448 code
    // points of which HotSpot agrees on 25. The tables below are the JDK's own
    // answers; `isEmojiModifier` keeps a table too, so a future Unicode bump
    // regenerates all five the same way instead of five different ways.
    registry.register("java/lang/Character", "isEmoji", "(I)Z", |_ctx, args| {
        let cp = match args.first() {
            Some(Value::Int(v)) => *v as u32,
            _ => 0,
        };
        let is_emoji = in_code_point_runs(JAVA_EMOJI_RUNS, cp);
        Ok(Some(Value::Int(if is_emoji { 1 } else { 0 })))
    });
    registry.register(
        "java/lang/Character",
        "isEmojiPresentation",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_ep = in_code_point_runs(JAVA_EMOJI_PRESENTATION_RUNS, cp);
            Ok(Some(Value::Int(if is_ep { 1 } else { 0 })))
        },
    );
    registry.register(
        "java/lang/Character",
        "isEmojiModifier",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_em = in_code_point_runs(JAVA_EMOJI_MODIFIER_RUNS, cp);
            Ok(Some(Value::Int(if is_em { 1 } else { 0 })))
        },
    );
    registry.register(
        "java/lang/Character",
        "isEmojiModifierBase",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_emb = in_code_point_runs(JAVA_EMOJI_MODIFIER_BASE_RUNS, cp);
            Ok(Some(Value::Int(if is_emb { 1 } else { 0 })))
        },
    );
    registry.register(
        "java/lang/Character",
        "isEmojiComponent",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_ec = in_code_point_runs(JAVA_EMOJI_COMPONENT_RUNS, cp);
            Ok(Some(Value::Int(if is_ec { 1 } else { 0 })))
        },
    );

    // -----------------------------------------------------------------------
    // Phase 12: Boolean additional methods
    // -----------------------------------------------------------------------
    registry.register(
        "java/lang/Boolean",
        "parseBoolean",
        "(Ljava/lang/String;)Z",
        native_boolean_parse_boolean,
    );
    registry.register(
        "java/lang/Boolean",
        "toString",
        "(Z)Ljava/lang/String;",
        native_boolean_static_to_string,
    );
    registry.register(
        "java/lang/Boolean",
        "hashCode",
        "(Z)I",
        native_boolean_static_hash_code,
    );
    registry.register(
        "java/lang/Boolean",
        "compare",
        "(ZZ)I",
        native_boolean_compare,
    );
    registry.register(
        "java/lang/Boolean",
        "getBoolean",
        "(Ljava/lang/String;)Z",
        native_boolean_get_boolean,
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// java.lang.Float / Double natives
// ---------------------------------------------------------------------------

pub(crate) fn native_float_to_raw_int_bits(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let f = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(f.to_bits() as i32)))
}

pub(crate) fn native_double_to_raw_long_bits(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let d = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Long(d.to_bits() as i64)))
}

pub(crate) fn native_long_bits_to_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let l = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Double(f64::from_bits(l as u64))))
}

// ---------------------------------------------------------------------------
// Step 5: Math natives
// ---------------------------------------------------------------------------

/// Extract an i64 from a Value that may carry a long-typed payload under
/// either the `Long` or `Double` CompactValue tag.  Long arguments crossing
/// the native-invocation boundary may arrive tagged as `Double` (CompactValue
/// stores untagged 64-bit values whose `tag()` returns `Double` whenever the
/// bit-pattern doesn't collide with a NaN-tag); reinterpret bits to recover
/// the original i64.  Same defensive pattern as `value_stack::pop_long`.
fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(v)) => i64::from_le_bytes(v.to_le_bytes()),
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

// --- abs ---
#[inline(always)]
pub(crate) fn native_math_abs_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(v.wrapping_abs())))
}

#[inline(always)]
pub(crate) fn native_math_abs_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = long_arg(args, 0);
    Ok(Some(Value::Long(v.wrapping_abs())))
}

#[inline(always)]
pub(crate) fn native_math_abs_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Float(v.abs())))
}

#[inline(always)]
pub(crate) fn native_math_abs_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.abs())))
}

// --- max ---
#[inline(always)]
pub(crate) fn native_math_max_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(std::cmp::max(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_max_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = long_arg(args, 0);
    let b = long_arg(args, 1);
    Ok(Some(Value::Long(std::cmp::max(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_max_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Float(java_max_float(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_max_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(java_max_double(a, b))))
}

// --- min ---
#[inline(always)]
pub(crate) fn native_math_min_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(std::cmp::min(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_min_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = long_arg(args, 0);
    let b = long_arg(args, 1);
    Ok(Some(Value::Long(std::cmp::min(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_min_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Float(java_min_float(a, b))))
}

#[inline(always)]
pub(crate) fn native_math_min_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(java_min_double(a, b))))
}

// ---------------------------------------------------------------------------
// The `StrictMath` bodies: fdlibm, not platform libm.
//
// Generated from one macro rather than written out eighteen times. That is not
// only brevity — eighteen hand-copied bodies differing by a single identifier
// is exactly the shape that produces a native bound to the wrong function, and
// a `StrictMath.cos` quietly answering `sin` would pass every accuracy test
// ever written. With the macro, the method name, the JVM signature and the
// fdlibm routine appear together on one line at each registration site above,
// and the argument marshalling has a single implementation.
//
// Every one is pure arithmetic on the argument `Value`s and never touches
// `ctx`, so none can allocate, safepoint, collect, or raise a JNI-pending
// exception — the same leaf property their `Math` counterparts have. Each is
// registered from exactly the block its `Math` twin is registered from, so the
// `set_leaf` state it inherits is unchanged by this split.
// ---------------------------------------------------------------------------

macro_rules! strict_math_unary {
    ($($rust_name:ident => $fdlibm_fn:ident),* $(,)?) => {
        $(
            #[doc = concat!(
                "`StrictMath.", stringify!($fdlibm_fn), "(double)` — fdlibm, not platform libm. ",
                "`StrictMath`'s contract is \"the fdlibm result, on every platform and every ",
                "VM\", so this body is mandatory there. Whether `java.lang.Math` ALSO gets it ",
                "is a per-function question answered by the census above `let strict` in ",
                "`register_math_natives`: HotSpot's `Math.f` is a one-line delegation to ",
                "`StrictMath.f`, so the two differ only where HotSpot substitutes an ",
                "intrinsic. Where it does not, `Math` is registered here too."
            )]
            #[inline]
            pub(crate) fn $rust_name(
                _ctx: &mut dyn NativeContext,
                args: &[Value],
            ) -> MethodCallResult {
                let v = match args.first() {
                    Some(Value::Double(v)) => *v,
                    _ => 0.0,
                };
                Ok(Some(Value::Double(cratonvm_types::fdlibm::$fdlibm_fn(v))))
            }
        )*
    };
}

macro_rules! strict_math_binary {
    ($($rust_name:ident => $fdlibm_fn:ident),* $(,)?) => {
        $(
            #[doc = concat!(
                "`StrictMath.", stringify!($fdlibm_fn), "(double, double)` — fdlibm, not ",
                "platform libm, and for `atan2`/`hypot` `java.lang.Math` as well. See the ",
                "note on the unary family."
            )]
            #[inline]
            pub(crate) fn $rust_name(
                _ctx: &mut dyn NativeContext,
                args: &[Value],
            ) -> MethodCallResult {
                let a = match args.first() {
                    Some(Value::Double(v)) => *v,
                    _ => 0.0,
                };
                let b = match args.get(1) {
                    Some(Value::Double(v)) => *v,
                    _ => 0.0,
                };
                Ok(Some(Value::Double(cratonvm_types::fdlibm::$fdlibm_fn(a, b))))
            }
        )*
    };
}

// These bodies back `StrictMath` unconditionally and `Math` for every row
// HotSpot does not intrinsify — see the census above `let strict`.
strict_math_unary! {
    native_fdlibm_sin => sin,
    native_fdlibm_cos => cos,
    native_fdlibm_tan => tan,
    native_fdlibm_asin => asin,
    native_fdlibm_acos => acos,
    native_fdlibm_atan => atan,
    native_fdlibm_exp => exp,
    native_fdlibm_log => log,
    native_fdlibm_log10 => log10,
    native_fdlibm_cbrt => cbrt,
    native_fdlibm_log1p => log1p,
    native_fdlibm_expm1 => expm1,
    native_fdlibm_sinh => sinh,
    native_fdlibm_cosh => cosh,
    native_fdlibm_tanh => tanh,
}

// Argument order here is load-bearing and is NOT the alphabetical one: the JDK
// declares `atan2(double y, double x)` — ordinate first — and `fdlibm::atan2`
// takes them in that same order, so `args[0]` is `y`. Transposing them is a
// defect no accuracy test can see, because `atan2(y, x)` and `atan2(x, y)` are
// both plausible angles; only the quadrant is wrong. The golden vectors in
// `types/src/fdlibm.rs` cross the full sign matrix, which is what catches it.
strict_math_binary! {
    native_fdlibm_atan2 => atan2,
    native_fdlibm_pow => pow,
    native_fdlibm_hypot => hypot,
}

/// `Math.max`/`Math.min` are NOT `f64::max`/`f64::min`.
///
/// Rust's IEEE-754-2019 `maxNum` semantics deliberately IGNORE a NaN operand
/// and return the other one; Java's `Math.max` PROPAGATES it. They also
/// disagree on signed zero, which Rust leaves unspecified and Java pins
/// (`max(+0.0, -0.0)` is `+0.0`, `min(+0.0, -0.0)` is `-0.0`).
///
/// The NaN half is not a corner case anybody has to go looking for: it is how
/// a `Double.NaN` sentinel travels through a fold. commons-math's
/// `StatUtilsTest.testMax` asserts `NaN` for an array containing one, and got
/// `-Infinity` — the fold's identity element — because every `max` step
/// silently dropped the NaN. The census that found it replayed a HotSpot
/// oracle over raw bit patterns; a generator that only draws finite values
/// cannot see this at all.
///
/// Bodies are transcribed from `java.lang.Math` in JDK 25, including the
/// return-`a`-not-`NAN` detail (it preserves the NaN payload) and the
/// asymmetry between `max` testing `a`'s sign bit and `min` testing `b`'s.
#[inline]
pub(crate) fn java_max_double(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == NEGATIVE_ZERO_DOUBLE_BITS {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

#[inline]
pub(crate) fn java_min_double(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == NEGATIVE_ZERO_DOUBLE_BITS {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

#[inline]
pub(crate) fn java_max_float(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == NEGATIVE_ZERO_FLOAT_BITS {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

#[inline]
pub(crate) fn java_min_float(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == NEGATIVE_ZERO_FLOAT_BITS {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

// --- W7-94: how wide the min/max rule actually is, and why nothing saw it ---
//
// Measured 2026-08-12 against HotSpot 25.0.3+9, same host, same class file,
// before the four helpers above existed:
//
//                             HotSpot   CratonVM (before)
//     Math.min(1.0, NaN)      NaN       1.0
//     Math.max(1.0, NaN)      NaN       1.0
//     Math.min(-0.0, 0.0)     -0.0      0.0
//     Math.min(1.0f, NaNf)    NaN       1.0
//     StrictMath.min(1.0,NaN) NaN       1.0
//
// while `min(II)I` and `min(JJ)J` were correct — the pass/fail boundary is per
// DESCRIPTOR, below the granularity any census reports.
//
// `register_math_natives` is called for BOTH `java/lang/Math` and
// `java/lang/StrictMath`, so four bodies were eight wrong triples, and
// `Float.min`/`max` inherit these with no registration of their own.
// `Double.min`/`max` do NOT — they carry their own bodies in `phases_early`,
// which is why that file calls the two aliases below.
//
// Why nothing caught it: the enclosing registrar opens with
// `set_category(NativeKind::Intrinsic)`, and `Intrinsic` is exempt from shadow
// retirement AND is not the census's `native-shadows-bytecode` kind — so a
// `--jdk-only-report` run of a program calling `Math.min` four times yields
// ZERO `java/lang/Math` rows. The correct tree already existed in-tree as
// `phases_late::streams::p56_java_math_min`, whose doc comment describes this
// exact trap, with one caller: the positive half fixed, the twin left.

/// `java.lang.Math.max(double,double)` under the name `phases_early`'s
/// `Double.max` body already calls. One rule, one implementation — see
/// [`java_max_double`], which this forwards to verbatim.
#[inline(always)]
pub(crate) fn java_math_max_f64(a: f64, b: f64) -> f64 {
    java_max_double(a, b)
}

/// `java.lang.Math.min(double,double)` — the mirror of [`java_math_max_f64`],
/// forwarding to [`java_min_double`].
#[inline(always)]
pub(crate) fn java_math_min_f64(a: f64, b: f64) -> f64 {
    java_min_double(a, b)
}

/// Raw bits of `-0.0`, matching `Math`'s own `negativeZeroDoubleBits`.
const NEGATIVE_ZERO_DOUBLE_BITS: u64 = 0x8000_0000_0000_0000;
/// Raw bits of `-0.0f`, matching `Math`'s own `negativeZeroFloatBits`.
const NEGATIVE_ZERO_FLOAT_BITS: u32 = 0x8000_0000;

// `Double`/`Float` value semantics — canonicalizing NaN for equality, hashing
// and ordering — live in `cratonvm_types::jfp`, transcribed once. They used to
// be four private helpers here, which is how `Comparator.naturalOrder()` in
// `native-collections` kept running `f64::total_cmp` after these were repaired.
use cratonvm_types::jfp::{
    double_compare as java_compare_double, float_compare as java_compare_float,
};
use cratonvm_types::jfp::{
    double_to_long_bits as double_to_long_bits_canonical,
    float_to_int_bits as float_to_int_bits_canonical,
};

// --- trig and math functions ---
#[inline(always)]
pub(crate) fn native_math_sqrt(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sqrt())))
}

#[inline]
pub(crate) fn native_math_pow(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // The rows where C99 `pow` is NOT `java.lang.Math.pow`. C returns 1.0 for
    // `pow(1, y)` at every `y`, NaN included; the JLS makes the second argument
    // dominant — "if the second argument is NaN, then the result is NaN", and
    // "if the absolute value of the first argument equals 1 and the second
    // argument is infinite, then the result is NaN". `f64::powf` is the C rule,
    // so those inputs came back as 1.0 where HotSpot returns NaN.
    //
    // The two NaN rules are deliberately NOT symmetric, because HotSpot's are
    // not (probes/MathSurfaceSweep, raw bit patterns): with an INFINITE exponent
    // both unit bases answer the canonical NaN, but with a NaN exponent only
    // `+1.0` does — `pow(-1.0, NaN)` hands back the NaN OPERAND with its payload,
    // which is what `powf` already does, so that row falls through untouched.
    // `b == 0.0` is excluded throughout: zero is neither NaN nor infinite, and
    // `powf(x, 0)` is already 1.0 for every `x`, NaN base included.
    if a.abs() == 1.0 && b.is_infinite() {
        return Ok(Some(Value::Double(f64::NAN)));
    }
    if a == 1.0 && b.is_nan() {
        return Ok(Some(Value::Double(f64::NAN)));
    }
    // A NaN BASE propagates with its sign bit; the libm we call clears it.
    if a.is_nan() && b != 0.0 {
        return Ok(Some(Value::Double(a)));
    }
    // Integer-exponent fast path, restricted to the four exponents where it is
    // EXACT.
    //
    // This used to run `a.powi(b as i32)` for every integer `b` with
    // `|b| < 64`, described as a "HotSpot-style fast path". HotSpot has no such
    // path — its `_dpow` intrinsic is the general Intel LIBM algorithm at every
    // exponent — and `powi` is repeated squaring, which rounds once per
    // multiply. `java.lang.Math.pow` is specified to be "within 1 ulp of the
    // exact result"; measured against a HotSpot JDK 25 oracle over 1040 bases
    // per exponent, `powi` was outside that from `b = 3` upward and the error
    // grew with the exponent:
    //
    // | exponent | disagrees | max ULP |
    // | ---      | ---       | ---     |
    // | ±1, 0, 2 | 0/1040    | 0       |
    // | 3        | 273/1040  | 1       |
    // | 8        | 783/1040  | 4       |
    // | 17       | 900/1040  | 9       |
    // | 31       | 980/1040  | 20      |
    // | 62       | 1008/1040 | 40      |
    // | ≥64      | 0/1040    | 0       |
    //
    // — 24026 of 35360 sampled `|b| < 64` inputs wrong, against 1 of 6240 for
    // the `powf` path just outside it. The exponents kept below are the ones
    // that round exactly once, so each is the correctly-rounded power and each
    // measured 0/1040: `x^0` is 1, `x^1` is `x`, `x^2` is a single multiply of
    // the exact product, and `x^-1` is a single divide. `x^-2` is NOT in the
    // list — `1/(x*x)` rounds twice and missed on 299 of 1040.
    if a.is_finite() && b.is_finite() && b.fract() == 0.0 {
        if b == 0.0 {
            return Ok(Some(Value::Double(1.0)));
        }
        if b == 1.0 {
            return Ok(Some(Value::Double(a)));
        }
        if b == 2.0 {
            return Ok(Some(Value::Double(a * a)));
        }
        if b == -1.0 {
            return Ok(Some(Value::Double(1.0 / a)));
        }
    }
    if a == 1.0 && b.is_nan() {
        return Ok(Some(Value::Double(f64::NAN)));
    }
    if a.is_nan() && b != 0.0 {
        return Ok(Some(Value::Double(a)));
    }
    // There used to be an integer-exponent fast path here that routed
    // `b.fract() == 0 && |b| < 64` to `powi`, i.e. to repeated multiplication.
    // It was not free: `probes/PowIntExpProbe` puts 400 bases against every
    // exponent in [-70, 70] and that shortcut disagreed with HotSpot on 36,947
    // of the 55,600 rows it owned — 66% — against 17 of ~800 on this `powf`
    // line. Repeated multiplication compounds one rounding per multiply, and
    // for a negative exponent a reciprocal on top. The census that chose libm
    // for this function (see the table above `let strict`) drew continuous
    // exponents, so it never priced the shortcut it was sitting behind.
    Ok(Some(Value::Double(a.powf(b))))
}

#[inline]
pub(crate) fn native_math_sin(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sin())))
}

#[inline]
pub(crate) fn native_math_cos(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cos())))
}

#[inline]
pub(crate) fn native_math_tan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.tan())))
}

#[inline]
pub(crate) fn native_math_log(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ln())))
}

#[inline]
pub(crate) fn native_math_log10(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // `log10` of a negative (including -Infinity) is NaN, but WHICH NaN is
    // observable: HotSpot's `_dlog10` stub yields the x86 default QNaN, which
    // has its SIGN BIT SET (0xFFF8...), while the libm we call here returns the
    // positive canonical NaN. `Math.log` already agrees with HotSpot; only
    // `log10` needed pinning. -0.0 is not `< 0.0`, so it still answers
    // -Infinity as specified.
    if v < 0.0 {
        return Ok(Some(Value::Double(f64::from_bits(0xFFF8_0000_0000_0000))));
    }
    Ok(Some(Value::Double(v.log10())))
}

#[inline]
pub(crate) fn native_math_exp(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.exp())))
}

#[inline(always)]
pub(crate) fn native_math_floor(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.floor())))
}

#[inline(always)]
pub(crate) fn native_math_ceil(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ceil())))
}

#[inline]
pub(crate) fn native_math_rint(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Java Math.rint: round to nearest even (banker's rounding).
    // The JDK body is `sign * abs(a)` with an early-out for large magnitudes,
    // so a NaN comes back through `Math.abs` — payload intact, SIGN CLEARED.
    // `round_ties_even` passes a negative NaN through unchanged, which is one
    // bit off the oracle.
    if v.is_nan() {
        return Ok(Some(Value::Double(v.abs())));
    }
    Ok(Some(Value::Double(v.round_ties_even())))
}

/// JDK 9+ Math.round(double) semantics (JDK-6430675).
///
/// The naive `floor(v + 0.5)` is WRONG for the largest value just below 0.5:
/// `0.49999999999999994 + 0.5` rounds up to exactly `1.0` in IEEE-754, so the
/// naive form returns 1 instead of the correct 0. OpenJDK fixed this by
/// computing the round-half-up result directly from the significand bits, which
/// avoids the spurious add. We mirror that exact algorithm so half-way and
/// just-below-half cases match HotSpot bit-for-bit.
#[inline]
pub(crate) fn round_double(a: f64) -> i64 {
    // Layout constants for IEEE-754 binary64.
    const SIGNIFICAND_WIDTH: i64 = 53; // 52 stored bits + implicit leading 1
    const EXP_BIAS: i64 = 1023;
    const EXP_BIT_MASK: u64 = 0x7FF0_0000_0000_0000;
    const SIGNIF_BIT_MASK: u64 = 0x000F_FFFF_FFFF_FFFF;

    let long_bits = a.to_bits();
    let biased_exp = ((long_bits & EXP_BIT_MASK) >> (SIGNIFICAND_WIDTH - 1)) as i64;
    let shift = (SIGNIFICAND_WIDTH - 2 + EXP_BIAS) - biased_exp;
    if (shift & -64) == 0 {
        // `a` is finite with 2^-64 <= ulp(a) < 1, i.e. shift in 0..=63.
        let mut r = ((long_bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1)) as i64;
        if (long_bits as i64) < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        // `a` is already integral (|a| >= 2^52), or +/-0, infinity, or NaN.
        // `as i64` is a saturating/NaN->0 cast, matching the JDK's clamping
        // semantics for out-of-range and NaN inputs.
        a as i64
    }
}

#[inline]
pub(crate) fn native_math_round_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Java Math.round(double) returns long. Use the bit-exact JDK 9+ algorithm
    // (round_double) instead of floor(v + 0.5), which mis-rounds the largest
    // value just below 0.5 (round(0.49999999999999994) must be 0, not 1).
    Ok(Some(Value::Long(round_double(v))))
}

/// JDK 9+ Math.round(float) semantics (JDK-6430675). Same bit-exact
/// round-half-up algorithm as `round_double`, but for IEEE-754 binary32, so the
/// largest float just below 0.5f rounds to 0 (not 1).
#[inline]
pub(crate) fn round_float(a: f32) -> i32 {
    // Layout constants for IEEE-754 binary32.
    const SIGNIFICAND_WIDTH: i32 = 24; // 23 stored bits + implicit leading 1
    const EXP_BIAS: i32 = 127;
    const EXP_BIT_MASK: u32 = 0x7F80_0000;
    const SIGNIF_BIT_MASK: u32 = 0x007F_FFFF;

    let int_bits = a.to_bits();
    let biased_exp = ((int_bits & EXP_BIT_MASK) >> (SIGNIFICAND_WIDTH - 1)) as i32;
    let shift = (SIGNIFICAND_WIDTH - 2 + EXP_BIAS) - biased_exp;
    if (shift & -32) == 0 {
        // `a` is finite with 2^-32 <= ulp(a) < 1, i.e. shift in 0..=31.
        let mut r = ((int_bits & SIGNIF_BIT_MASK) | (SIGNIF_BIT_MASK + 1)) as i32;
        if (int_bits as i32) < 0 {
            r = -r;
        }
        ((r >> shift) + 1) >> 1
    } else {
        // `a` is already integral (|a| >= 2^23), or +/-0, infinity, or NaN.
        // `as i32` is a saturating/NaN->0 cast, matching the JDK's clamping.
        a as i32
    }
}

#[inline]
pub(crate) fn native_math_round_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // Java Math.round(float) returns int. Use the bit-exact JDK 9+ algorithm
    // (round_float) instead of floor(v + 0.5f), which mis-rounds the largest
    // value just below 0.5f.
    Ok(Some(Value::Int(round_float(v))))
}

#[inline]
pub(crate) fn native_math_to_radians(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.to_radians())))
}

#[inline]
pub(crate) fn native_math_to_degrees(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.to_degrees())))
}

thread_local! {
    static MATH_RANDOM_SEED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[inline]
fn thread_id_u64() -> u64 {
    // No stable `ThreadId::as_u64`; use a process-wide counter assigned per thread.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TID: AtomicU64 = AtomicU64::new(1);
    thread_local! { static TID: u64 = NEXT_TID.fetch_add(1, Ordering::Relaxed); }
    TID.with(|t| *t)
}

#[inline]
fn init_seed_if_zero() -> u64 {
    MATH_RANDOM_SEED.with(|c| {
        let v = c.get();
        if v != 0 {
            return v;
        }
        // Lazy first-touch seed from time + thread id so threads don't all
        // start with the same constant.
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5DEECE66D);
        let tid = thread_id_u64();
        let seed = now_ns.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(tid);
        let seed = if seed == 0 { 0x5DEECE66D } else { seed };
        c.set(seed);
        seed
    })
}

pub(crate) fn native_math_random(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let next = MATH_RANDOM_SEED.with(|c| {
        let mut v = c.get();
        if v == 0 {
            v = init_seed_if_zero();
        }
        // SplitMix64 — advance state then mix.
        let new = v.wrapping_add(0x9E3779B97F4A7C15);
        c.set(new);
        let mut z = new;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    });
    // Convert to [0.0, 1.0) double: take top 53 bits.
    let bits = (next >> 11) as f64 / ((1u64 << 53) as f64);
    Ok(Some(Value::Double(bits)))
}

#[inline]
pub(crate) fn native_math_signum_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // `Math.signum` is `(d == 0.0 || Double.isNaN(d)) ? d : copySign(1.0, d)`
    // — it returns the ARGUMENT for NaN, not a fresh canonical NaN, so the
    // payload and sign bit survive. Returning `f64::NAN` here turned every NaN
    // into `7ff8000000000000`; HotSpot hands back the bits it was given.
    let result = if v.is_nan() || v == 0.0 {
        v
    } else if v > 0.0 {
        1.0
    } else {
        -1.0
    };
    Ok(Some(Value::Double(result)))
}

#[inline]
pub(crate) fn native_math_signum_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // See `native_math_signum_double`: the argument comes back unchanged for
    // NaN and for both zeroes.
    let result = if v.is_nan() || v == 0.0 {
        v
    } else if v > 0.0 {
        1.0
    } else {
        -1.0
    };
    Ok(Some(Value::Float(result)))
}

#[inline]
pub(crate) fn native_math_cbrt(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cbrt())))
}

#[inline]
pub(crate) fn native_math_ieee_remainder(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // fdlibm, for BOTH `Math` and `StrictMath`. Unlike the rest of this file's
    // transcendentals there is no accuracy latitude to trade away here: both
    // specs read "as prescribed by the IEEE 754 standard", which fixes the
    // result exactly, so a deviating `Math.IEEEremainder` is as wrong as a
    // deviating `StrictMath.IEEEremainder`.
    //
    // The previous body was `a - (a/b).round() * b`, guarded on a few specials.
    // That is wrong two independent ways:
    //
    //   1. `f64::round` is ties-AWAY-from-zero. IEEE 754 requires the quotient
    //      rounded to the NEAREST integer with TIES TO EVEN. Every half-integer
    //      quotient came out with the wrong remainder — `IEEEremainder(1.5, 1)`
    //      returned `0.5` where the answer is `-0.5`.
    //   2. `a / b` overflows to infinity for operands whose remainder is
    //      perfectly representable (`IEEEremainder(MAX_VALUE, MIN_NORMAL)`),
    //      and `a - inf*b` is then NaN. fdlibm never forms the quotient at all:
    //      it reduces by `fmod` against `2p` and finishes with two conditional
    //      subtractions, so the result is exact by construction.
    //
    // Measured: 49.83% of sampled pairs disagreed with HotSpot, with UNBOUNDED
    // error — by a wide margin the worst row in the census, and the only one
    // that was not a last-ULP story. W7-54-strictmath-fdlibm-family.md.
    Ok(Some(Value::Double(cratonvm_types::fdlibm::ieee_remainder(
        a, b,
    ))))
}

// ---------------------------------------------------------------------------
// Phase 13 Step 1: Math exact arithmetic
// ---------------------------------------------------------------------------

pub(crate) fn math_overflow_err() -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::ArithmeticException {
        message: "integer overflow".to_string(),
    }
    .into()
}

/// Overflow for the `long`-typed exact-arithmetic ops. The JDK throws
/// `ArithmeticException("long overflow")` from `Math.{add,subtract,multiply,
/// negate,increment,decrement}Exact(long…)`, distinct from the int variants'
/// "integer overflow". ES `ByteSizeValue` addition asserts on the exact
/// message ("long overflow"), so the int message broke testAddition.
pub(crate) fn math_overflow_err_long() -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::ArithmeticException {
        message: "long overflow".to_string(),
    }
    .into()
}

#[inline]
pub(crate) fn native_math_add_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_add(b) {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_add_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_add(b) {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

#[inline]
pub(crate) fn native_math_subtract_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_sub(b) {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_subtract_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_sub(b) {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

#[inline]
pub(crate) fn native_math_multiply_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_mul(b) {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_multiply_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_mul(b) {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

#[inline]
pub(crate) fn native_math_increment_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_add(1) {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_increment_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_add(1) {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

#[inline]
pub(crate) fn native_math_decrement_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_sub(1) {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_decrement_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_sub(1) {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

#[inline]
pub(crate) fn native_math_negate_exact_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_neg() {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_negate_exact_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_neg() {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err_long()),
    }
}

// ---------------------------------------------------------------------------
// Phase 13 Step 2: Floor/ceil division + toIntExact
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn native_math_floor_div_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(cratonvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    // Java floorDiv: rounds toward negative infinity. `wrapping_*` is not a
    // shortcut here, it is the SPECIFIED behaviour: javadoc says that for
    // `floorDiv(Integer.MIN_VALUE, -1)` "integer overflow occurs and the result
    // is equal to Integer.MIN_VALUE" — the same wraparound `idiv` gives. Plain
    // `/` and `%` are checked in Rust and PANIC on that pair, which aborts the
    // whole VM process instead of returning a value.
    let d = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) {
        d.wrapping_sub(1)
    } else {
        d
    };
    Ok(Some(Value::Int(result)))
}

#[inline]
pub(crate) fn native_math_floor_div_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(cratonvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    // See `native_math_floor_div_int` for why these are `wrapping_*`:
    // `floorDiv(Long.MIN_VALUE, -1L)` is specified to overflow to
    // `Long.MIN_VALUE`, and a checked `/` panics there.
    let d = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) {
        d.wrapping_sub(1)
    } else {
        d
    };
    Ok(Some(Value::Long(result)))
}

#[inline]
pub(crate) fn native_math_floor_mod_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(cratonvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    // Java floorMod: a - floorDiv(a,b) * b. `wrapping_rem` for the same reason
    // as `floorDiv`: `MIN_VALUE % -1` panics under a checked `%` even though the
    // mathematical answer (0) is representable.
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
    Ok(Some(Value::Int(result)))
}

#[inline]
pub(crate) fn native_math_floor_mod_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(cratonvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
    Ok(Some(Value::Long(result)))
}

#[inline]
pub(crate) fn native_math_to_int_exact(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match i32::try_from(v) {
        Ok(i) => Ok(Some(Value::Int(i))),
        Err(_) => Err(math_overflow_err()),
    }
}

#[inline]
pub(crate) fn native_math_multiply_high(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let result = ((a as i128) * (b as i128)) >> 64;
    Ok(Some(Value::Long(result as i64)))
}

/// `Math.unsignedMultiplyHigh(long, long)` — high 64 bits of the *unsigned*
/// 128-bit product (added in JDK 18). HotSpot intrinsifies this to a single
/// `mulx`; the JDK fallback bytecode is a multi-op Hacker's-Delight routine.
/// It is the single hottest leaf in the SunEC P-256 Montgomery field multiply
/// (`MontgomeryIntegerPolynomialP256.mult` calls it once per limb pair), so
/// interpreting it dominates EC keygen/sign/verify time. Byte-identical to the
/// JDK by construction: both compute `(a·b mod 2^128) >> 64` over unsigned a,b.
#[inline]
pub(crate) fn native_math_unsigned_multiply_high(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    let result = ((a as u128) * (b as u128)) >> 64;
    Ok(Some(Value::Long(result as u64 as i64)))
}

// ---------------------------------------------------------------------------
// Phase 13 Step 3: Advanced math functions
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn native_math_tanh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.tanh())))
}

#[inline]
pub(crate) fn native_math_copy_sign_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mag = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let sign = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(mag.copysign(sign))))
}

#[inline]
pub(crate) fn native_math_copy_sign_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mag = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let sign = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Float(mag.copysign(sign))))
}

/// `StrictMath.copySign(double, double)` — a NaN sign argument counts as
/// positive. See the registration comment in `register_math_natives`.
#[inline]
pub(crate) fn native_strict_copy_sign_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mag = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let sign = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let sign = if sign.is_nan() { 1.0 } else { sign };
    Ok(Some(Value::Double(mag.copysign(sign))))
}

/// `StrictMath.copySign(float, float)` — see the double overload. Both widths
/// were wrong: a family fix that took only the `double` form would have left
/// the `(FF)F` row red.
#[inline]
pub(crate) fn native_strict_copy_sign_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mag = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let sign = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let sign = if sign.is_nan() { 1.0 } else { sign };
    Ok(Some(Value::Float(mag.copysign(sign))))
}

#[inline]
pub(crate) fn native_math_next_up_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let result = if v.is_nan() {
        // The JDK returns the argument itself, so both the sign bit and the
        // payload survive; `f64::NAN` canonicalised them away.
        v
    } else if v == f64::INFINITY {
        f64::INFINITY
    } else if v == 0.0 {
        // Both +0.0 and -0.0 → smallest positive subnormal
        f64::from_bits(1)
    } else {
        let bits = v.to_bits();
        if v > 0.0 {
            f64::from_bits(bits + 1)
        } else {
            f64::from_bits(bits - 1)
        }
    };
    Ok(Some(Value::Double(result)))
}

#[inline]
pub(crate) fn native_math_next_down_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let result = if v.is_nan() {
        // See `native_math_next_up_double`: the argument is returned as-is.
        v
    } else if v == f64::NEG_INFINITY {
        f64::NEG_INFINITY
    } else if v == 0.0 {
        // Both +0.0 and -0.0 → largest negative subnormal
        -f64::from_bits(1)
    } else {
        let bits = v.to_bits();
        if v > 0.0 {
            f64::from_bits(bits - 1)
        } else {
            f64::from_bits(bits + 1)
        }
    };
    Ok(Some(Value::Double(result)))
}

#[inline]
pub(crate) fn native_math_next_after(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let direction = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let result = if start.is_nan() || direction.is_nan() {
        // The JDK writes exactly `return start + direction;` here, which is not
        // the same as a canonical NaN: whichever operand is the NaN propagates
        // with its sign and payload. (When BOTH are NaN the answer is whatever
        // the add picks, which is a register-allocation detail on x86 and is
        // left unspecified by the JLS — the sweep excludes those rows.)
        start + direction
    } else if start == direction {
        direction
    } else if direction > start {
        // next up
        if start == 0.0 {
            f64::from_bits(1)
        } else {
            let bits = start.to_bits();
            if start > 0.0 {
                f64::from_bits(bits + 1)
            } else {
                f64::from_bits(bits - 1)
            }
        }
    } else {
        // next down
        if start == 0.0 {
            -f64::from_bits(1)
        } else {
            let bits = start.to_bits();
            if start > 0.0 {
                f64::from_bits(bits - 1)
            } else {
                f64::from_bits(bits + 1)
            }
        }
    };
    Ok(Some(Value::Double(result)))
}

#[inline]
pub(crate) fn native_math_ulp_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // W7-95(C1). `Math.ulp` switches on the exponent and returns `Math.abs(d)`
    // for the whole `MAX_EXPONENT + 1` case — NaN and both infinities under one
    // arm. For a NaN that means the argument with its sign bit cleared and its
    // payload INTACT, not the canonical quiet NaN `f64::NAN` would have
    // produced. Measured on HotSpot 25:
    //
    //   Math.ulp(0x7ff0000000000001) = 0x7ff0000000000001   (payload kept)
    //   Math.ulp(0xfff8000000000000) = 0x7ff8000000000000   (sign cleared)
    //   Math.ulp(0xffc8ae0a)         = 0x7fc8ae0a           (float width; was
    //                                                        0x7fc00000 here)
    //
    // Only "is NaN" is specified, so the payload half is fidelity rather than a
    // contract — but `v.abs()` is both the JDK's own expression and strictly
    // closer to it, so there is no reason to write anything else.
    let result = if v.is_nan() || v.is_infinite() {
        v.abs()
    } else {
        let abs = v.abs();
        if abs == f64::MAX {
            // Stepping UP from MAX_VALUE lands on infinity, so `next - abs` was
            // infinity — but `ulp(±Double.MAX_VALUE)` is specified as 2^971.
            // At the top of the range the gap below equals the gap above, so
            // step DOWN instead; the subtraction is exact.
            abs - f64::from_bits(abs.to_bits() - 1)
        } else {
            let next = f64::from_bits(abs.to_bits() + 1);
            next - abs
        }
    };
    Ok(Some(Value::Double(result)))
}

#[inline]
pub(crate) fn native_math_ulp_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // See `native_math_ulp_double`: one `Math.abs` arm covers NaN and both
    // infinities, and it keeps a NaN payload.
    //
    // EXHAUSTIVELY VERIFIED, this width: all 4,294,967,296 `float` bit patterns
    // were run through this algorithm and through `Math.ulp` on HotSpot 25 and
    // compared as `floatToRawIntBits`. Every non-NaN pattern — 4,278,190,082 of
    // them — already matched; the 16,777,212 that did not were exactly the
    // non-canonical NaNs this change fixes. There is no residual defect at this
    // width: do not "fix" it again.
    let result = if v.is_nan() || v.is_infinite() {
        v.abs()
    } else {
        let abs = v.abs();
        if abs == f32::MAX {
            // See `native_math_ulp_double`: `ulp(±Float.MAX_VALUE)` is 2^104,
            // not the infinity that stepping up produces.
            abs - f32::from_bits(abs.to_bits() - 1)
        } else {
            let next = f32::from_bits(abs.to_bits() + 1);
            next - abs
        }
    };
    Ok(Some(Value::Float(result)))
}

#[inline]
pub(crate) fn native_math_get_exponent_double(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // The JDK is ONE expression with no special cases (Math.java):
    //
    //     ((int)((doubleToRawLongBits(d) & EXP_BIT_MASK) >> 52)) - 1023
    //
    // so a SUBNORMAL reports the same -1023 as zero: `getExponent` returns the
    // unbiased exponent FIELD, not the value's mathematical exponent. The
    // hand-written branches here computed the latter for subnormals --
    // MEASURED 2026-08-13 (scratchpad/orch/Exp.java):
    // `getExponent(Double.MIN_VALUE)` was -1074 where HotSpot answers -1023.
    // The float form has no native at all and runs the real JDK bytecode,
    // which is why it was already right; this one had a hand-rolled twin.
    // Every other case the branches enumerated (zero, NaN, Infinity,
    // MIN_NORMAL, 1.0) falls out of the same subtraction, verified against the
    // oracle -- so they were not merely redundant, they were the only reason
    // the wrong branch looked plausible.
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let result = if biased == 0x7FF {
        // NaN or Infinity → MAX_EXPONENT + 1
        1024
    } else if biased == 0 {
        // Zero AND subnormal both answer MIN_EXPONENT - 1 == -1023. This branch
        // used to normalize the significand and report the TRUE exponent of a
        // subnormal (-1074 for Double.MIN_VALUE), which is the mathematically
        // interesting number but not the specified one: `Math.getExponent` reads
        // "the unbiased exponent used in the REPRESENTATION", and a subnormal's
        // stored exponent field is 0 for all of them.
        -1023
    } else {
        biased - 1023
    };
    Ok(Some(Value::Int(result)))
}

// ---------------------------------------------------------------------------
// Step 6: Wrapper type boxing/unboxing
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Cached wrapper ClassIds
//
// Every autobox (Integer.valueOf, Boolean.valueOf, Long.valueOf, ...) would
// otherwise call `ctx.ensure_class_initialized("java/lang/Integer")` per
// invocation. Cache the resolved ClassIds per VM/heap identity: Rust tests can
// create multiple independent `Vm` instances in one process, and a ClassId from
// one VM must not be reused in another.
type WrapperCidCache = std::collections::HashMap<(usize, &'static str), u32>;

static WRAPPER_CIDS: std::sync::OnceLock<parking_lot::Mutex<WrapperCidCache>> =
    std::sync::OnceLock::new();

fn wrapper_cids() -> &'static parking_lot::Mutex<WrapperCidCache> {
    WRAPPER_CIDS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

fn wrapper_cid_key(class_name: &str) -> Option<&'static str> {
    match class_name {
        "java/lang/Integer" => Some("java/lang/Integer"),
        "java/lang/Long" => Some("java/lang/Long"),
        "java/lang/Float" => Some("java/lang/Float"),
        "java/lang/Double" => Some("java/lang/Double"),
        "java/lang/Boolean" => Some("java/lang/Boolean"),
        "java/lang/Byte" => Some("java/lang/Byte"),
        "java/lang/Short" => Some("java/lang/Short"),
        "java/lang/Character" => Some("java/lang/Character"),
        _ => None,
    }
}

/// Helper: allocate a wrapper object with 1 field using a well-known class name.
/// Falls back to ClassId(0) with 1 field if the class can't be loaded.
///
/// For the 8 well-known wrapper classes (Integer, Long, Float, Double,
/// Boolean, Byte, Short, Character) the resolved ClassId is cached by VM
/// identity after the first successful `ensure_class_initialized`, so repeated
/// autoboxes in the same VM skip the name lookup.
pub(crate) fn alloc_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> cratonvm_types::ObjectRef {
    // Fast path: hit the VM-scoped wrapper-CID cache for well-known names.
    if let Some(key) = wrapper_cid_key(class_name) {
        let scope = ctx.vm_identity();
        let cached = wrapper_cids()
            .lock()
            .get(&(scope, key))
            .copied()
            .unwrap_or(0);
        if cached != 0 {
            return ctx.alloc_object(cratonvm_types::ClassId::new(cached), 1);
        }
        // First call: resolve, cache, then allocate.
        if let Ok(class_id) = ctx.ensure_class_initialized(class_name) {
            // ClassId::new(0) is the synthetic fallback sentinel — never
            // cache it (it would defeat the cache miss path).
            let raw = class_id.as_u32();
            if raw != 0 {
                wrapper_cids().lock().insert((scope, key), raw);
            }
            return ctx.alloc_object(class_id, 1);
        }
        return ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    }
    // Non-cached class name (caller used a non-wrapper name).
    match ctx.ensure_class_initialized(class_name) {
        Ok(class_id) => ctx.alloc_object(class_id, 1),
        Err(_) => ctx.alloc_object(cratonvm_types::ClassId::new(0), 1),
    }
}

// Round-9 CRIT GC-correctness fix: BOOLEAN_CACHE and INTEGER_CACHE must be
// shared across threads, NOT thread-local. WP4.6 additionally scopes the cached
// ObjectRefs by VM identity because Rust tests can create multiple independent
// heaps in one process. Two reasons:
//   (1) JLS § 5.1.7 requires `Boolean.TRUE == Boolean.TRUE` and
//       `Integer.valueOf(n) == Integer.valueOf(n)` for n in [-128,127]
//       across ALL threads in the same VM (the boxing conversion produces one
//       canonical cached instance per primitive value). A thread-local cache makes
//       these identity comparisons fail when the two operands come from
//       different threads.
//   (2) Under a moving GC, thread-local ObjectRefs that are *not* scanned
//       as roots on every thread point at stale (collected or relocated)
//       addresses. We now (a) hold the cache in a VM-scoped process table and
//       (b) wire
//       `gc_scan_value_of_cache_roots` / `gc_update_value_of_cache_refs`
//       into `vm/src/memory/{roots.rs,gc.rs}` so the cached entries are
//       both kept live and re-pointed after compaction.
type ScopedValueCache<const N: usize> =
    std::collections::HashMap<usize, [Option<cratonvm_types::ObjectRef>; N]>;

/// The one cache in this file whose upper bound is **configurable**, so its
/// backing store cannot be a `[Option<ObjectRef>; N]` like the other five.
///
/// See [`integer_cache_bound`] for the rule and the measurement. The `Vec` is
/// sized once, at bound-resolution time, and never resized afterwards — so an
/// index computed against the latched bound is always in range.
type ScopedIntegerCache = std::collections::HashMap<usize, Vec<Option<cratonvm_types::ObjectRef>>>;

/// LOCK LEVEL (lock-discipline ratchet): `Scratch`, like the rest of this
/// family. Safe only BECAUSE `INTEGER_CACHE_HIGH` is unordered:
/// `resolve_integer_cache_high` holds that guard across this acquisition. If
/// `INTEGER_CACHE_HIGH` is ever given a level it must be a HIGHER one, never an
/// equal — see its own comment for why it has none today.
static INTEGER_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedIntegerCache>,
> = std::sync::OnceLock::new();

fn integer_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedIntegerCache> {
    INTEGER_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `IntegerCache.low` — `-128`, and NOT configurable. `jdk25src/java.base/
/// java/lang/Integer.java`: `static final int low = -128;` is a literal with
/// no property behind it, and only `high` reads one.
const INTEGER_CACHE_LOW: i32 = -128;

/// The property `IntegerCache.<clinit>` reads. HotSpot reads it through
/// `jdk.internal.misc.VM.getSavedProperty`, not `System.getProperty` — which
/// is why `System.getProperty("java.lang.Integer.IntegerCache.high")` answers
/// **`null`** on HotSpot even in a run where the cache really was widened
/// (MEASURED: `prop.System=null` under `-Djava.lang.Integer.IntegerCache
/// .high=1000` *and* under `-XX:AutoBoxCacheMax=1000`, while `int.1000`
/// answered `true` in both). CratonVM has no saved-property split; `-D` lands
/// in `shared.system_properties`, which is what `get_system_property` reads,
/// and it is populated from `VmConfig` before any bytecode runs.
const INTEGER_CACHE_HIGH_PROPERTY: &str = "java.lang.Integer.IntegerCache.high";

/// The resolved `IntegerCache.high` for one VM, latched on first use.
///
/// VM-scoped rather than a process-global `OnceLock`: a `OnceLock` latches the
/// FIRST VM's answer for the lifetime of the process, and this crate's Rust
/// tests build several independent VMs in one binary.
///
/// NO LOCK LEVEL, deliberately — and it is the one member of this family that
/// could not take one. `resolve_integer_cache_high` holds this guard across
/// `integer_cache().lock()`, so this is the OUTER lock of a two-lock nest and
/// `INTEGER_CACHE` is the inner one. A level is a claim that no lock at or
/// below it is held when this one is taken, and the ordering it implies runs
/// the other way: the inner lock would have to sit BELOW this one. `Scratch`
/// is the floor, so stamping `Scratch` here would demand a level that cannot
/// exist, and any higher level would be a claim about the VM's own hierarchy
/// that this cache has no business making.
///
/// The nesting is load-bearing, not incidental: `high` and the matching
/// `entries` must be installed atomically or two racing threads can publish a
/// `high` whose cache is the wrong length. So the fix is to restructure the
/// publish, not to relabel the lock — the same verdict `java_recordings` and
/// `boot_layer_memo` got, and it stays in the §A6 backlog for the same reason.
static INTEGER_CACHE_HIGH: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<usize, i32>>,
> = std::sync::OnceLock::new();

fn integer_cache_high() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, i32>> {
    INTEGER_CACHE_HIGH.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// `IntegerCache.high`'s value from a raw property string, or `None` to keep
/// the default.
///
/// Transliterated from `jdk25src/java.base/java/lang/Integer.java`:
///
/// ```text
/// h = Math.max(parseInt(v), 127);
/// h = Math.min(h, Integer.MAX_VALUE - (-low) - 1);
/// ... catch (NumberFormatException nfe) { /* ignore it */ }
/// ```
///
/// Three rules, each of which has its own measured row and none of which is
/// guessable from the other two (all on OpenJDK 25.0.3+9, `CacheHigh.java`):
///
/// * `=1000` widens to `-128..=1000`: `int.1000` true, `int.1001` false.
/// * `=50` does **not** narrow: `Math.max(.., 127)` floors it, and `int.128`
///   stays false while `int.127` stays true. A reader who implemented only
///   "high = parsed" would make a *narrowing* configuration observable, which
///   HotSpot never does.
/// * `=abc` is ignored, not fatal: `int.128` false, the run completes.
///
/// The parse is [`java_parse_signed`], not `str::parse` — the property is read
/// by `Integer.parseInt`, whose grammar accepts a leading `+` and rejects
/// surrounding whitespace, and this file already owns that grammar.
fn parse_integer_cache_high(raw: &str) -> Option<i32> {
    match java_parse_signed(raw, 10, i32::MIN as i64, i32::MAX as i64) {
        JavaIntParse::Ok(v) => {
            // `Math.max(parsed, 127)` then `Math.min(h, MAX_VALUE - 128 - 1)`.
            let h = (v as i32).max(127);
            Some(h.min(i32::MAX - (-INTEGER_CACHE_LOW) - 1))
        }
        // `NumberFormatException` on both arms — `Integer.parseInt` raises it
        // for a malformed string AND for a well-formed out-of-int-range one,
        // and `IntegerCache`'s `catch` swallows both identically.
        JavaIntParse::Malformed | JavaIntParse::OutOfRange => None,
    }
}

/// The `IntegerCache.high` in force for this VM, resolving and latching it on
/// the first call.
///
/// Latching on first use is HotSpot's own timing, not an approximation of it:
/// `IntegerCache.high` is a `static final` assigned in `IntegerCache
/// .<clinit>`, which runs at the first autobox in the VM's life and never
/// again. A later `System.setProperty` does not move HotSpot's bound and does
/// not move this one.
///
/// **Lock order is memo → cache, and only here.** Every other reader takes
/// `integer_cache()` alone; nothing takes the cache lock and then this memo,
/// so the pair cannot deadlock.
///
/// The `try_reserve_exact` is not defensive padding. `high` is permitted up to
/// `Integer.MAX_VALUE - 129`, i.e. a backing store of ~17 GB; HotSpot answers
/// that configuration with an `OutOfMemoryError` from `new Integer[...]`, but
/// a `vec![None; len]` here would **abort the process**, which is strictly
/// worse than any Java outcome. On a refusal the bound falls back to the JDK
/// default rather than to something in between, so the VM stays in a state the
/// oracle can also produce.
fn integer_cache_bound(ctx: &mut dyn NativeContext) -> i32 {
    let scope = ctx.vm_identity();
    if let Some(high) = integer_cache_high().lock().get(&scope).copied() {
        return high;
    }
    let mut high = 127i32;
    if let Some(raw) = ctx.get_system_property(INTEGER_CACHE_HIGH_PROPERTY) {
        if let Some(parsed) = parse_integer_cache_high(&raw) {
            high = parsed;
        }
    }
    let mut entries: Vec<Option<cratonvm_types::ObjectRef>> = Vec::new();
    let len = (high as i64 - INTEGER_CACHE_LOW as i64 + 1) as usize;
    if entries.try_reserve_exact(len).is_err() {
        high = 127;
        entries = vec![None; 256];
    } else {
        entries.resize(len, None);
    }
    let mut memo = integer_cache_high().lock();
    if let Some(existing) = memo.get(&scope).copied() {
        return existing;
    }
    integer_cache().lock().entry(scope).or_insert(entries);
    memo.insert(scope, high);
    high
}

static BOOLEAN_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<2>>,
> = std::sync::OnceLock::new();

fn boolean_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<2>> {
    BOOLEAN_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `LongCache` for `Long.valueOf(long)` — JLS §5.1.7 mandates the canonical
/// cached instance for values in [-128, 127] so `Long.valueOf(x) ==
/// Long.valueOf(x)` holds. Shared across threads but VM-scoped and GC-scanned
/// for the same reasons documented above for `INTEGER_CACHE`.
static LONG_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>>,
> = std::sync::OnceLock::new();

fn long_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>> {
    LONG_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

// ---------------------------------------------------------------------------
// The three caches the family was MISSING, and why their bounds all differ.
//
// The boxing caches are NOT one rule applied eight times. Each bound below was
// read out of `jdk25src/java.base/java/lang/*.java` and then MEASURED against
// Microsoft OpenJDK 25.0.3+9 (`BoxOracle`, every code unit / every byte / the
// whole short range walked, not sampled):
//
//   Character  `if (c <= 127) return CharacterCache.cache[c];`  -> 0..=127.
//              First non-identical code unit measured on HotSpot: 128.
//   Byte       `return ByteCache.cache[b + 128];` — UNCONDITIONAL. Every one
//              of the 256 byte values is canonical; `Byte.valueOf` has no
//              fresh-allocation arm at all. Measured: all 256 identical.
//   Short      `if (sAsInt >= -128 && sAsInt <= 127)` -> -128..=127, measured
//              by walking Short.MIN_VALUE..Short.MAX_VALUE (exactly that range
//              came back identical).
//
// And the members that are deliberately NOT here:
//
//   Integer    -128..=IntegerCache.high (127 by default) — already correct in
//              `native_integer_value_of`; NOT widened here.
//   Long       -128..=127 — already correct in `native_long_value_of`.
//   Boolean    exactly two, and they must be the `Boolean.TRUE`/`FALSE` STATIC
//              FIELDS, not privately minted twins (see the long comment on
//              `native_boolean_value_of`).
//   Float      no cache. `Float.valueOf(0f) == Float.valueOf(0f)` is FALSE on
//   Double     HotSpot, measured. Adding a cache for these would be a
//              regression, not a completion of the family — the asymmetry is
//              the specification.
// ---------------------------------------------------------------------------

/// `CharacterCache` for `Character.valueOf(char)`. 128 slots indexed by the
/// code unit itself — there is no offset because the low bound is zero.
static CHARACTER_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<128>>,
> = std::sync::OnceLock::new();

fn character_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<128>> {
    CHARACTER_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `ByteCache` for `Byte.valueOf(byte)`. 256 slots indexed by `b + 128`, and
/// unlike every other cache in this file it covers the type's ENTIRE domain.
static BYTE_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>>,
> = std::sync::OnceLock::new();

fn byte_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>> {
    BYTE_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `ShortCache` for `Short.valueOf(short)`. 256 slots indexed by `s + 128`,
/// covering -128..=127 out of a 65,536-value domain.
static SHORT_CACHE: std::sync::OnceLock<
    cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>>,
> = std::sync::OnceLock::new();

fn short_cache() -> &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<256>> {
    SHORT_CACHE.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The canonical-instance dance, once, for the caches added above.
///
/// Returns the cached wrapper for `idx`, allocating and installing it on the
/// first call. The lock is DROPPED across `alloc_wrapper` (which can run
/// `<clinit>` and can GC), so the post-allocation re-check under the lock is
/// load-bearing: two threads that miss together must still agree on which
/// instance is canonical, or `==` breaks for exactly the values the JLS says
/// it must hold for. The loser's allocation is unreachable and collectible.
fn cached_wrapper_box<const N: usize>(
    ctx: &mut dyn NativeContext,
    cache: &'static cratonvm_types::lock_order::OrderedPlMutex<ScopedValueCache<N>>,
    idx: usize,
    class_name: &'static str,
    value: Value,
) -> cratonvm_types::ObjectRef {
    let scope = ctx.vm_identity();
    if let Some(cached) = {
        let c = cache.lock();
        c.get(&scope).and_then(|entries| entries[idx])
    } {
        return cached;
    }
    let obj = alloc_wrapper(ctx, class_name);
    ctx.set_field(obj, 0, value);
    // `alloc_wrapper` falls back to `ClassId(0)` when the wrapper class cannot
    // be initialised — which can only happen in a bootstrap window, but these
    // caches are process-global and never invalidated, so installing one of
    // those would latch a wrong-classed instance as THE canonical box for the
    // rest of the VM's life. Decline to cache instead: the caller still gets a
    // usable object, and the value simply goes uncached until the class is
    // real, which is the pre-fix behaviour rather than a new failure.
    if ctx.class_id_of_object(obj).as_u32() == 0 {
        return obj;
    }
    let mut guard = cache.lock();
    let entries = guard.entry(scope).or_insert([None; N]);
    if let Some(existing) = entries[idx] {
        return existing;
    }
    entries[idx] = Some(obj);
    obj
}

/// Report one cache's live entries for `vm_identity` to the GC.
///
/// Factored out when the family grew from three caches to six. The per-cache
/// copy-pasted block is precisely how a new cache gets added to the root scan
/// and forgotten in the remap below (or the reverse): a cache that is rooted
/// but not re-pointed is a use-after-move that only appears after a compacting
/// collection, and the canonical instances are by construction long-lived
/// enough to be moved.
/// The generic is `AsRef<[Option<ObjectRef>]>`, not `const N: usize`, so that
/// the ONE scan covers both backing shapes: the five fixed-bound caches'
/// `[Option<ObjectRef>; N]` and `INTEGER_CACHE`'s `Vec` (whose length depends
/// on `IntegerCache.high`). Both `[T; N]` and `Vec<T>` satisfy it, so the six
/// call sites below are unchanged and no cache can acquire a second, separate
/// hook — which is the failure this function was factored out to prevent.
fn scan_one_cache<C: AsRef<[Option<cratonvm_types::ObjectRef>]>>(
    cache: &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, C>>,
    vm_identity: usize,
    out: &mut Vec<cratonvm_types::ObjectRef>,
) {
    let cache = cache.lock();
    if let Some(entries) = cache.get(&vm_identity) {
        for slot in entries.as_ref().iter().flatten() {
            out.push(*slot);
        }
    }
}

/// Remap one cache's entries for `vm_identity` through the GC pointer map.
fn update_one_cache<C: AsMut<[Option<cratonvm_types::ObjectRef>]>>(
    cache: &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, C>>,
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    let mut cache = cache.lock();
    if let Some(entries) = cache.get_mut(&vm_identity) {
        for obj_ref in entries.as_mut().iter_mut().flatten() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

/// GC root scan hook — called from `vm/src/memory/roots.rs::collect_roots`.
/// Reports cached wrapper ObjectRefs for the active VM so the GC keeps them live.
pub fn gc_scan_value_of_cache_roots(vm_identity: usize, out: &mut Vec<cratonvm_types::ObjectRef>) {
    scan_one_cache(integer_cache(), vm_identity, out);
    scan_one_cache(boolean_cache(), vm_identity, out);
    scan_one_cache(long_cache(), vm_identity, out);
    scan_one_cache(character_cache(), vm_identity, out);
    scan_one_cache(byte_cache(), vm_identity, out);
    scan_one_cache(short_cache(), vm_identity, out);
}

/// GC post-compaction hook — called from `vm/src/memory/gc.rs::update_all_roots`.
/// Remaps every cached entry for the active VM through the GC's pointer map.
pub fn gc_update_value_of_cache_refs(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    update_one_cache(integer_cache(), vm_identity, pointer_map);
    update_one_cache(boolean_cache(), vm_identity, pointer_map);
    update_one_cache(long_cache(), vm_identity, pointer_map);
    update_one_cache(character_cache(), vm_identity, pointer_map);
    update_one_cache(byte_cache(), vm_identity, pointer_map);
    update_one_cache(short_cache(), vm_identity, pointer_map);
}

/// The canonical wrapper for `(desc, v)` **if one is already cached**, without
/// allocating, initialising a class, or populating anything.
///
/// This exists for exactly one caller shape: code on the `&SharedVm` side of
/// the native boundary — `vm/src/vm/vm_exec.rs`'s proxy argument boxing — which
/// has no `&mut dyn NativeContext` and therefore cannot call
/// `native_integer_value_of` and friends at all. HotSpot answers those paths
/// canonically (MEASURED, F19-1 §2: `proxy.int`/`char`/`bool`/`long`/`byte`/
/// `short` all `true`), and today they allocate. Rather than mint a second
/// `IntegerCache` over there, this reads the SIX caches that already exist here
/// and are already wired into `gc_scan_value_of_cache_roots` /
/// `gc_update_value_of_cache_refs` as one `VmRootSource { scan, remap }` pair.
///
/// **Read-only is a correctness requirement, not a performance one.**
/// Populating a cache needs `alloc_wrapper`, which needs
/// `ensure_class_initialized`, which runs `<clinit>` — and a proxy invocation
/// is not a legal place to trigger class initialisation. So a miss is `None`
/// and the caller keeps its existing allocation. A `None` must never be turned
/// into a `null` argument; that is the defect recorded above
/// `lang_class::create_method_object`.
///
/// **The `Value` variant is matched as well as the descriptor, and that is the
/// load-bearing half.** A `long` slot can legitimately present as a compact
/// `Value::Int` — the shape `native_wrapper_long_value` exists to widen. A
/// descriptor-only match would answer `("J", Value::Int(5))` with the cached
/// `Long.valueOf(0)`: an identity fix converted into a **wrong answer**, which
/// is worse than the defect it fixes. Every mismatched pair falls through to
/// `None`, i.e. to today's fresh box carrying the right value.
///
/// `"Z"` is deliberately absent, and it is the one arm a reader would expect
/// and must not add. `Boolean.valueOf` returns the live `Boolean.TRUE`/`FALSE`
/// **static fields**, not a privately minted twin (see
/// [`native_boolean_value_of`]); `BOOLEAN_CACHE` is only its bootstrap
/// fallback, so an entry in it is not guaranteed to be the instance the rest
/// of the VM calls canonical. `vm_exec.rs` resolves the statics directly
/// (`proxy_canonical_boolean`) and needs nothing from here. `"F"`/`"D"` are
/// absent because HotSpot caches neither (`neg.floatValueOf` = false);
/// "completing the family to eight" is a regression, not a completion.
pub fn canonical_wrapper_if_cached(
    vm_identity: usize,
    desc: &str,
    v: Value,
) -> Option<cratonvm_types::ObjectRef> {
    fn read<C: AsRef<[Option<cratonvm_types::ObjectRef>]>>(
        cache: &'static cratonvm_types::lock_order::OrderedPlMutex<
            std::collections::HashMap<usize, C>,
        >,
        vm_identity: usize,
        idx: usize,
    ) -> Option<cratonvm_types::ObjectRef> {
        let guard = cache.lock();
        guard
            .get(&vm_identity)
            .and_then(|entries| entries.as_ref().get(idx).copied().flatten())
    }
    match (desc, v) {
        // `IntegerCache`'s upper bound is configurable, so the bound is not
        // checked here at all: the backing store's LENGTH is the bound, and
        // `read`'s `get(idx)` is exactly that test. Reading the latched bound
        // instead would need the memo, and a VM that has not boxed an `int`
        // yet has neither — which is a miss either way.
        ("I", Value::Int(x)) if x >= INTEGER_CACHE_LOW => {
            // Widen before subtracting: `x` is unbounded ABOVE here (the
            // `get(idx)` below is the only bound), so `i32::MAX -
            // INTEGER_CACHE_LOW` overflows an `i32` and panics a debug build
            // where it must simply MISS. Both store-SIZING sites in this file
            // already widen to `i64` for exactly this reason.
            read(
                integer_cache(),
                vm_identity,
                (x as i64 - INTEGER_CACHE_LOW as i64) as usize,
            )
        }
        ("J", Value::Long(x)) if (-128..=127).contains(&x) => {
            read(long_cache(), vm_identity, (x + 128) as usize)
        }
        // `CharacterCache` has no negative half: `if (c <= 127)` indexes by
        // the code unit itself, so the offset the other three use is absent.
        ("C", Value::Int(x)) if (0..=127).contains(&x) => {
            read(character_cache(), vm_identity, x as usize)
        }
        // `ByteCache` is unconditional over all 256 byte values, but a raw
        // `Value::Int` in a `B` slot can carry anything, so the range test
        // stays — it is a domain check here, not a cache-bound check.
        ("B", Value::Int(x)) if (-128..=127).contains(&x) => {
            read(byte_cache(), vm_identity, (x + 128) as usize)
        }
        ("S", Value::Int(x)) if (-128..=127).contains(&x) => {
            read(short_cache(), vm_identity, (x + 128) as usize)
        }
        _ => None,
    }
}

pub(crate) fn native_integer_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // `high` is `IntegerCache.high`, which is CONFIGURABLE — see
    // [`integer_cache_bound`]. `low` is not. This is the only member of the
    // family whose bound is not a literal, and widening it must not drag the
    // others: MEASURED under `-Djava.lang.Integer.IntegerCache.high=1000`,
    // `int.1000` is `true` while `long.128`, `short.128` and `char.128` are
    // all still `false`.
    let high = integer_cache_bound(ctx);
    if (INTEGER_CACHE_LOW..=high).contains(&val) {
        let idx = (val - INTEGER_CACHE_LOW) as usize;
        let scope = ctx.vm_identity();
        // Fast path: lock, read, drop lock before any heap allocation.
        // `entries.get(idx)` rather than `entries[idx]`: the bound is latched
        // per VM and the store is sized to it, so a miss here is impossible —
        // but an indexing panic inside a native is a VM abort, and a `None`
        // is an extra allocation.
        if let Some(cached) = {
            let c = integer_cache().lock();
            c.get(&scope)
                .and_then(|entries| entries.get(idx).copied().flatten())
        } {
            return Ok(Some(Value::Object(Some(cached))));
        }
        let obj = alloc_wrapper(ctx, "java/lang/Integer");
        ctx.set_field(obj, 0, Value::Int(val));
        // Re-check under the lock — another thread may have populated the
        // slot while we were allocating. If so, drop ours and return theirs
        // (the loser allocation is collectible — but the race is rare and
        // it preserves the JLS identity invariant).
        let mut cache = integer_cache().lock();
        let entries = cache
            .entry(scope)
            .or_insert_with(|| vec![None; (high as i64 - INTEGER_CACHE_LOW as i64 + 1) as usize]);
        if let Some(slot) = entries.get_mut(idx) {
            if let Some(existing) = *slot {
                return Ok(Some(Value::Object(Some(existing))));
            }
            *slot = Some(obj);
        }
        return Ok(Some(Value::Object(Some(obj))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Integer");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_integer_get_integer_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let default = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let value = match args.first() {
        Some(Value::Object(Some(name_obj))) => ctx
            .read_string(*name_obj)
            .and_then(|name| ctx.get_system_property(&name))
            .and_then(|raw| raw.trim().parse::<i32>().ok())
            .unwrap_or(default),
        _ => default,
    };
    native_integer_value_of(ctx, &[Value::Int(value)])
}

/// Shared unboxing for Integer.intValue(), Boolean.booleanValue(),
/// Character.charValue(), Byte.byteValue(), Short.shortValue().
pub(crate) fn native_wrapper_int_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = ctx.get_field(this, 0);
    match val {
        Value::Int(_) => Ok(Some(val)),
        _ => Ok(Some(Value::Int(0))),
    }
}

// ---------------------------------------------------------------------------
// Java's integer grammar
//
// `str::parse` is NOT `Integer.parseInt`. The two differ in BOTH directions,
// and every difference is a silently wrong answer rather than an error:
//
//   * Rust's parser rejects the non-ASCII decimal digits `Character.digit`
//     accepts. Measured on JDK 25:
//     `Integer.parseInt("\u{661}\u{662}") == 12` (ARABIC-INDIC ONE TWO).
//   * A `.trim()` in front of the parse invents an acceptance Java does not
//     have. `Integer.parseInt("  1")`, `("1 ")`, `("1\n")` all throw
//     `NumberFormatException` on a real JDK; we answered 1. The `.trim()` is
//     `Double.parseDouble`'s contract — whose grammar really does skip
//     `[\x00-\x20]*` on both ends — borrowed onto the integer one, where it
//     does not belong.
//
// Both directions matter for input validation: code that calls `parseInt` in
// a `try` to reject junk was getting junk accepted.
// ---------------------------------------------------------------------------

/// The non-ASCII runs of `Character.digit`, generated from JDK 25 itself by
/// walking every code point and recording each maximal run over which
/// `Character.digit(cp, 36)` increases by one. Entries are
/// `(first, last, value_at_first)`.
///
/// BMP only, deliberately. `Integer.parseInt` walks the string with `charAt`
/// and calls the `char` overload of `Character.digit`, so a SUPPLEMENTARY
/// decimal digit arrives as a surrogate pair and matches nothing. Measured on
/// JDK 25: `Integer.parseInt(new String(Character.toChars(0x104A0)))` throws
/// even though `Character.digit(0x104A0, 10) == 0`. Adding the supplementary
/// runs here would make us MORE permissive than Java, not less.
const JAVA_DIGIT_RUNS: &[(u32, u32, u32)] = &[
    (0x0660, 0x0669, 0),
    (0x06F0, 0x06F9, 0),
    (0x07C0, 0x07C9, 0),
    (0x0966, 0x096F, 0),
    (0x09E6, 0x09EF, 0),
    (0x0A66, 0x0A6F, 0),
    (0x0AE6, 0x0AEF, 0),
    (0x0B66, 0x0B6F, 0),
    (0x0BE6, 0x0BEF, 0),
    (0x0C66, 0x0C6F, 0),
    (0x0CE6, 0x0CEF, 0),
    (0x0D66, 0x0D6F, 0),
    (0x0DE6, 0x0DEF, 0),
    (0x0E50, 0x0E59, 0),
    (0x0ED0, 0x0ED9, 0),
    (0x0F20, 0x0F29, 0),
    (0x1040, 0x1049, 0),
    (0x1090, 0x1099, 0),
    (0x17E0, 0x17E9, 0),
    (0x1810, 0x1819, 0),
    (0x1946, 0x194F, 0),
    (0x19D0, 0x19D9, 0),
    (0x1A80, 0x1A89, 0),
    (0x1A90, 0x1A99, 0),
    (0x1B50, 0x1B59, 0),
    (0x1BB0, 0x1BB9, 0),
    (0x1C40, 0x1C49, 0),
    (0x1C50, 0x1C59, 0),
    (0xA620, 0xA629, 0),
    (0xA8D0, 0xA8D9, 0),
    (0xA900, 0xA909, 0),
    (0xA9D0, 0xA9D9, 0),
    (0xA9F0, 0xA9F9, 0),
    (0xAA50, 0xAA59, 0),
    (0xABF0, 0xABF9, 0),
    (0xFF10, 0xFF19, 0),
    (0xFF21, 0xFF3A, 10),
    (0xFF41, 0xFF5A, 10),
];

/// `Character.digit(char, radix)` — the `char` overload, which is the one the
/// `parse*` family calls.
///
/// This is NOT `char::to_digit`: that handles only ASCII `0-9A-Za-z` and, for
/// a radix above 36, PANICS. This returns `None` instead of panicking and
/// covers the Unicode decimal runs above.
fn java_char_digit(c: char, radix: u32) -> Option<u32> {
    let cp = c as u32;
    // Fast path: the ASCII runs, which is all any hot call site ever sees.
    let v = if cp.wrapping_sub('0' as u32) < 10 {
        cp - '0' as u32
    } else if cp.wrapping_sub('a' as u32) < 26 {
        cp - 'a' as u32 + 10
    } else if cp.wrapping_sub('A' as u32) < 26 {
        cp - 'A' as u32 + 10
    } else if cp < JAVA_DIGIT_RUNS[0].0 {
        return None;
    } else {
        let mut found = None;
        for &(first, last, base) in JAVA_DIGIT_RUNS {
            if (first..=last).contains(&cp) {
                found = Some(base + (cp - first));
                break;
            }
        }
        found?
    };
    if v < radix {
        Some(v)
    } else {
        None
    }
}

/// Outcome of a Java integer parse, split because the JDK raises two DIFFERENT
/// `NumberFormatException` detail messages and only `Byte`/`Short` use the
/// second one.
pub(crate) enum JavaIntParse {
    Ok(i64),
    /// Not a well-formed `Signopt Digit+` in this radix.
    Malformed,
    /// Well-formed, but outside `[min, max]`.
    OutOfRange,
}

/// The `Integer.parseInt` / `Long.parseLong` grammar:
///
/// ```text
/// Signopt Digit+
/// ```
///
/// where `Digit` is anything `Character.digit(c, radix)` accepts. No
/// whitespace is permitted anywhere — not leading, not trailing, not either.
///
/// Accumulation is in `i128` against a signed magnitude limit, so
/// `MIN_VALUE` (whose magnitude is one larger than `MAX_VALUE`'s) parses
/// exactly and nothing can overflow: once the accumulator passes the limit we
/// stop accumulating but KEEP SCANNING, because a later non-digit still makes
/// the whole string malformed rather than out-of-range.
pub(crate) fn java_parse_signed(text: &str, radix: u32, min: i64, max: i64) -> JavaIntParse {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return JavaIntParse::Malformed; // ""
    };
    let (neg, mut pending) = match first {
        '+' => (false, None),
        '-' => (true, None),
        c => (false, Some(c)),
    };
    let limit: i128 = if neg { -(min as i128) } else { max as i128 };
    let mut acc: i128 = 0;
    let mut digits = 0usize;
    let mut over = false;
    loop {
        let c = match pending.take() {
            Some(c) => c,
            None => match chars.next() {
                Some(c) => c,
                None => break,
            },
        };
        let Some(d) = java_char_digit(c, radix) else {
            return JavaIntParse::Malformed;
        };
        digits += 1;
        if !over {
            acc = acc * radix as i128 + d as i128;
            if acc > limit {
                over = true;
            }
        }
    }
    if digits == 0 {
        return JavaIntParse::Malformed; // "+", "-"
    }
    if over {
        return JavaIntParse::OutOfRange;
    }
    let signed: i128 = if neg { -acc } else { acc };
    JavaIntParse::Ok(signed as i64)
}

/// The JDK's `NumberFormatException.forInputString` detail message. The
/// ` under radix N` tail is present for every radix except 10.
fn java_nfe_for_input(text: &str, radix: u32) -> cratonvm_types::error::MethodCallFailed {
    let message = if radix == 10 {
        format!("For input string: \"{text}\"")
    } else {
        format!("For input string: \"{text}\" under radix {radix}")
    };
    cratonvm_types::error::RuntimeError::NumberFormatException { message }.into()
}

/// The JDK's `Byte.parseByte` / `Short.parseShort` out-of-range message, which
/// is NOT the `forInputString` one.
fn java_nfe_out_of_range(text: &str, radix: u32) -> cratonvm_types::error::MethodCallFailed {
    cratonvm_types::error::RuntimeError::NumberFormatException {
        message: format!("Value out of range. Value:\"{text}\" Radix:{radix}"),
    }
    .into()
}

/// Read argument 0 as a non-null `String`, raising the JDK's
/// `NumberFormatException("Cannot parse null string")` for a null receiver —
/// which is what the integer family throws. (The FLOATING-point family throws
/// `NullPointerException` instead; see `read_string_arg_npe`.)
fn read_string_arg_nfe(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Ok(ctx.read_string(*obj).unwrap_or_default()),
        _ => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: "Cannot parse null string".to_string(),
        }
        .into()),
    }
}

/// Shared body for the whole signed `parse*(String[, int])` family.
///
/// `range_message` selects which of the JDK's two detail messages an
/// out-of-range value gets: `Integer`/`Long` report `forInputString`,
/// `Byte`/`Short` report `Value out of range`.
fn java_parse_into(
    text: &str,
    radix: u32,
    min: i64,
    max: i64,
    range_message: bool,
) -> Result<i64, cratonvm_types::error::MethodCallFailed> {
    match java_parse_signed(text, radix, min, max) {
        JavaIntParse::Ok(v) => Ok(v),
        JavaIntParse::Malformed => Err(java_nfe_for_input(text, radix)),
        JavaIntParse::OutOfRange => Err(if range_message {
            java_nfe_out_of_range(text, radix)
        } else {
            java_nfe_for_input(text, radix)
        }),
    }
}

pub(crate) fn native_integer_parse_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let v = java_parse_into(&text, 10, i32::MIN as i64, i32::MAX as i64, false)?;
    Ok(Some(Value::Int(v as i32)))
}

/// Validate a `parse*(String, int)` radix the way the JDK's `Integer.parseInt`
/// family does, returning the JDK's own exception detail message on rejection.
///
/// This is NOT the `toString(…, int)` rule. Measured against real JDK 25:
/// `Integer.parseInt` / `Long.parseLong` / `Byte.parseByte` /
/// `Short.parseShort` / `Integer.parseUnsignedInt` / `Long.parseUnsignedLong`
/// all THROW `NumberFormatException` for a radix outside 2..=36 —
/// `"radix 0 less than Character.MIN_RADIX"`,
/// `"radix 37 greater than Character.MAX_RADIX"` — whereas the `toString`
/// family silently substitutes 10 (see `crate::java_radix_or_ten`). Do not
/// merge the two: they are different contracts on the same argument.
///
/// The guard is also a safety requirement, not just a fidelity one. Rust's
/// `<int>::from_str_radix` PANICS ("must lie in the range `[2, 36]`") when the
/// radix is out of range, and the radix used to reach it via `*v as u32`, so a
/// negative radix arrived as a huge `u32`. `Integer.parseInt("5", 0)` from
/// ordinary Java bytecode therefore aborted the whole VM.
pub(crate) fn java_parse_radix_or_nfe(radix: i32) -> Result<u32, String> {
    if radix < 2 {
        Err(format!("radix {radix} less than Character.MIN_RADIX"))
    } else if radix > 36 {
        Err(format!("radix {radix} greater than Character.MAX_RADIX"))
    } else {
        Ok(radix as u32)
    }
}

/// Read argument 1 as a `parse*` radix, mapping an out-of-range value to the
/// `NumberFormatException` the JDK raises. A missing/mistyped argument keeps
/// the historical default of 10.
fn parse_radix_arg(args: &[Value]) -> Result<u32, cratonvm_types::error::MethodCallFailed> {
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 10,
    };
    java_parse_radix_or_nfe(radix).map_err(|message| {
        cratonvm_types::error::RuntimeError::NumberFormatException { message }.into()
    })
}

pub(crate) fn native_integer_parse_int_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let radix = parse_radix_arg(args)?;
    let v = java_parse_into(&text, radix, i32::MIN as i64, i32::MAX as i64, false)?;
    Ok(Some(Value::Int(v as i32)))
}

// --- Byte.parseByte ---

/// `Byte.parseByte` / `Short.parseShort` are two-step in the JDK: they call
/// `Integer.parseInt` and THEN range-check. That ordering is observable in the
/// detail message — a value outside `int` reports `forInputString`, while one
/// that fits an `int` but not the narrower type reports `Value out of range`.
fn java_parse_narrow(
    text: &str,
    radix: u32,
    min: i64,
    max: i64,
) -> Result<i64, cratonvm_types::error::MethodCallFailed> {
    let v = java_parse_into(text, radix, i32::MIN as i64, i32::MAX as i64, false)?;
    if !(min..=max).contains(&v) {
        return Err(java_nfe_out_of_range(text, radix));
    }
    Ok(v)
}

pub(crate) fn native_byte_parse_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let v = java_parse_narrow(&text, 10, i8::MIN as i64, i8::MAX as i64)?;
    Ok(Some(Value::Int(v as i32)))
}

pub(crate) fn native_byte_parse_byte_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let radix = parse_radix_arg(args)?;
    let v = java_parse_narrow(&text, radix, i8::MIN as i64, i8::MAX as i64)?;
    Ok(Some(Value::Int(v as i32)))
}

// --- Short.parseShort ---

pub(crate) fn native_short_parse_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let v = java_parse_narrow(&text, 10, i16::MIN as i64, i16::MAX as i64)?;
    Ok(Some(Value::Int(v as i32)))
}

pub(crate) fn native_short_parse_short_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let radix = parse_radix_arg(args)?;
    let v = java_parse_narrow(&text, radix, i16::MIN as i64, i16::MAX as i64)?;
    Ok(Some(Value::Int(v as i32)))
}

pub(crate) fn native_integer_to_hex_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // DEBUG-NETTYHANG: log every call
    if crate::nbflags().dbg_tohex {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 600 || n % 1000 == 0 {
            eprintln!("[DBG-TOHEX {n}] val={val}");
        }
    }
    let hex = format!("{:x}", val as u32);
    let result = ctx.create_string(&hex);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_integer_nlz(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.leading_zeros() as i32)))
}

pub(crate) fn native_integer_ntz(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.trailing_zeros() as i32)))
}

pub(crate) fn native_integer_bit_count(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.count_ones() as i32)))
}

pub(crate) fn native_integer_reverse(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.reverse_bits() as i32)))
}

pub(crate) fn native_integer_reverse_bytes(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.swap_bytes() as i32)))
}

// --- Long ---

pub(crate) fn native_long_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    if (-128..=127).contains(&val) {
        let idx = (val + 128) as usize;
        let scope = ctx.vm_identity();
        // Fast path: lock, read, drop lock before any heap allocation.
        if let Some(cached) = {
            let c = long_cache().lock();
            c.get(&scope).and_then(|entries| entries[idx])
        } {
            return Ok(Some(Value::Object(Some(cached))));
        }
        let obj = alloc_wrapper(ctx, "java/lang/Long");
        ctx.set_field(obj, 0, Value::Long(val));
        // Re-check under the lock — another thread may have populated the
        // slot while we were allocating. If so, drop ours and return theirs
        // (the loser allocation is collectible — but the race is rare and
        // it preserves the JLS identity invariant).
        let mut cache = long_cache().lock();
        let entries = cache.entry(scope).or_insert([None; 256]);
        if let Some(existing) = entries[idx] {
            return Ok(Some(Value::Object(Some(existing))));
        }
        entries[idx] = Some(obj);
        return Ok(Some(Value::Object(Some(obj))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Long");
    ctx.set_field(obj, 0, Value::Long(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_long_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let val = ctx.get_field(this, 0);
    match val {
        Value::Long(_) => Ok(Some(val)),
        // Reflection can legally widen an int-valued raw field slot while
        // constructing a Long wrapper. Preserve that JVM numeric conversion
        // instead of exposing the compact Int tag bits to a `()J` caller.
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

fn native_long_get_long_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let default = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let value = match args.first() {
        Some(Value::Object(Some(name_obj))) => ctx
            .read_string(*name_obj)
            .and_then(|name| ctx.get_system_property(&name))
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .unwrap_or(default),
        _ => default,
    };
    native_long_value_of(ctx, &[Value::Long(value)])
}

pub(crate) fn native_long_parse_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let v = java_parse_into(&text, 10, i64::MIN, i64::MAX, false)?;
    Ok(Some(Value::Long(v)))
}

pub(crate) fn native_long_nlz(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Int(v.leading_zeros() as i32)))
}

pub(crate) fn native_long_ntz(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Int(v.trailing_zeros() as i32)))
}

pub(crate) fn native_long_bit_count(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Int(v.count_ones() as i32)))
}

pub(crate) fn native_long_reverse(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Long(v.reverse_bits() as i64)))
}

pub(crate) fn native_long_reverse_bytes(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Long(v.swap_bytes() as i64)))
}

// --- Boolean ---

pub(crate) fn native_boolean_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let idx = if val != 0 { 1 } else { 0 };

    // JLS §5.1.7 / `Boolean.valueOf(boolean)` contract: the method returns
    // the canonical `Boolean.TRUE` / `Boolean.FALSE` static-field instances
    // (real JDK source is literally `return b ? TRUE : FALSE`). Code that
    // does reference-identity checks against `Boolean.TRUE` — most notably
    // Xerces' `XML11Configuration.configurePipeline()`, which decides
    // between the namespace-aware and non-namespace scanner via
    // `fFeatures.get("…/namespaces") == Boolean.TRUE` — depends on this.
    //
    // The previous implementation minted its own `BOOLEAN_CACHE` instances,
    // which were NOT identical to the `Boolean.TRUE`/`FALSE` objects created
    // by `Boolean.<clinit>`, so `valueOf(true) == Boolean.TRUE` was false.
    // Resolve and return the actual static fields instead. `ensure_class_
    // initialized` runs `<clinit>`, so `TRUE`/`FALSE` are populated by the
    // time we read them.
    if let Ok(class_id) = ctx.ensure_class_initialized("java/lang/Boolean") {
        let field_name = if idx == 1 { "TRUE" } else { "FALSE" };
        if let Some(field_idx) = ctx.static_field_index_by_name(class_id, field_name) {
            if let Value::Object(Some(o)) = ctx.get_static_field(class_id, field_idx) {
                // Return the live static field directly. The field is a GC
                // root in its own right (class statics are scanned), so no
                // private mirror is needed and none can go stale.
                return Ok(Some(Value::Object(Some(o))));
            }
        }
    }

    // Fallback (Boolean class somehow unavailable / fields not yet set):
    // keep the canonical-cache behaviour so repeated calls are at least
    // self-consistent.
    let scope = ctx.vm_identity();
    if let Some(o) = {
        let c = boolean_cache().lock();
        c.get(&scope).and_then(|entries| entries[idx])
    } {
        return Ok(Some(Value::Object(Some(o))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Boolean");
    ctx.set_field(obj, 0, Value::Int(idx as i32));
    let mut cache = boolean_cache().lock();
    let entries = cache.entry(scope).or_insert([None; 2]);
    if let Some(existing) = entries[idx] {
        return Ok(Some(Value::Object(Some(existing))));
    }
    entries[idx] = Some(obj);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// W7-95(C1) / W7-98 — java.lang.Character's tables are JAVA's, not Rust's.
//
// Every table below was GENERATED by walking `0..=0x10FFFF` on Microsoft
// OpenJDK 25.0.3+9 and recording the maximal runs over which the JDK's own
// answer holds, exactly as `JAVA_DIGIT_RUNS` above was. None of them is copied
// from Unicode data files, so none of them can be a transcription of the wrong
// Unicode version: the provenance is the oracle the differential is run
// against.
//
// The reason this cannot be delegated to a Rust `char` method is that Java's
// predicates are deliberately NOT Unicode's:
//
//   * `char::is_alphabetic` is the Unicode **Alphabetic** property, which is
//     `L* u Nl u Other_Alphabetic`. `Character.isLetter` is exactly the five
//     `L*` categories. Measured on JDK 25: 949 BMP and 1,731 total code points
//     are Alphabetic and not letters — and the census measured **957** BMP
//     disagreements against the shipping binary, so Rust's Alphabetic table
//     and JDK 25's differ by eight code points on top of the definitional gap.
//     Deriving `isLetter` as `is_alphabetic() && !delta` would therefore have
//     left a residual that nothing in this tree can name. The direct table
//     leaves none.
//   * Rust has no emoji predicates at all; the five that were here were
//     hand-written coarse ranges, and they were wrong on 2,746 code points.
// ---------------------------------------------------------------------------

/// Is `cp` inside one of a sorted, non-overlapping list of inclusive ranges?
fn in_code_point_runs(runs: &[(u32, u32)], cp: u32) -> bool {
    runs.binary_search_by(|&(lo, hi)| {
        if hi < cp {
            std::cmp::Ordering::Less
        } else if lo > cp {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// The value a `(first, last, value_at_first)` run assigns to `cp`, where the
/// value increases by one across the run.
fn value_in_runs(runs: &[(u32, u32, i32)], cp: u32) -> Option<i32> {
    runs.binary_search_by(|&(lo, hi, _)| {
        if hi < cp {
            std::cmp::Ordering::Less
        } else if lo > cp {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .ok()
    .map(|i| {
        let (lo, _, base) = runs[i];
        base + (cp - lo) as i32
    })
}

/// `Character.isLetter` - the five `L*` general categories, over EVERY plane.
/// 677 runs, 141028 code points.
#[rustfmt::skip]
const JAVA_LETTER_RUNS: &[(u32, u32)] = &[
    (0x0041, 0x005A), (0x0061, 0x007A), (0x00AA, 0x00AA), (0x00B5, 0x00B5), (0x00BA, 0x00BA),
    (0x00C0, 0x00D6), (0x00D8, 0x00F6), (0x00F8, 0x02C1), (0x02C6, 0x02D1), (0x02E0, 0x02E4),
    (0x02EC, 0x02EC), (0x02EE, 0x02EE), (0x0370, 0x0374), (0x0376, 0x0377), (0x037A, 0x037D),
    (0x037F, 0x037F), (0x0386, 0x0386), (0x0388, 0x038A), (0x038C, 0x038C), (0x038E, 0x03A1),
    (0x03A3, 0x03F5), (0x03F7, 0x0481), (0x048A, 0x052F), (0x0531, 0x0556), (0x0559, 0x0559),
    (0x0560, 0x0588), (0x05D0, 0x05EA), (0x05EF, 0x05F2), (0x0620, 0x064A), (0x066E, 0x066F),
    (0x0671, 0x06D3), (0x06D5, 0x06D5), (0x06E5, 0x06E6), (0x06EE, 0x06EF), (0x06FA, 0x06FC),
    (0x06FF, 0x06FF), (0x0710, 0x0710), (0x0712, 0x072F), (0x074D, 0x07A5), (0x07B1, 0x07B1),
    (0x07CA, 0x07EA), (0x07F4, 0x07F5), (0x07FA, 0x07FA), (0x0800, 0x0815), (0x081A, 0x081A),
    (0x0824, 0x0824), (0x0828, 0x0828), (0x0840, 0x0858), (0x0860, 0x086A), (0x0870, 0x0887),
    (0x0889, 0x088E), (0x08A0, 0x08C9), (0x0904, 0x0939), (0x093D, 0x093D), (0x0950, 0x0950),
    (0x0958, 0x0961), (0x0971, 0x0980), (0x0985, 0x098C), (0x098F, 0x0990), (0x0993, 0x09A8),
    (0x09AA, 0x09B0), (0x09B2, 0x09B2), (0x09B6, 0x09B9), (0x09BD, 0x09BD), (0x09CE, 0x09CE),
    (0x09DC, 0x09DD), (0x09DF, 0x09E1), (0x09F0, 0x09F1), (0x09FC, 0x09FC), (0x0A05, 0x0A0A),
    (0x0A0F, 0x0A10), (0x0A13, 0x0A28), (0x0A2A, 0x0A30), (0x0A32, 0x0A33), (0x0A35, 0x0A36),
    (0x0A38, 0x0A39), (0x0A59, 0x0A5C), (0x0A5E, 0x0A5E), (0x0A72, 0x0A74), (0x0A85, 0x0A8D),
    (0x0A8F, 0x0A91), (0x0A93, 0x0AA8), (0x0AAA, 0x0AB0), (0x0AB2, 0x0AB3), (0x0AB5, 0x0AB9),
    (0x0ABD, 0x0ABD), (0x0AD0, 0x0AD0), (0x0AE0, 0x0AE1), (0x0AF9, 0x0AF9), (0x0B05, 0x0B0C),
    (0x0B0F, 0x0B10), (0x0B13, 0x0B28), (0x0B2A, 0x0B30), (0x0B32, 0x0B33), (0x0B35, 0x0B39),
    (0x0B3D, 0x0B3D), (0x0B5C, 0x0B5D), (0x0B5F, 0x0B61), (0x0B71, 0x0B71), (0x0B83, 0x0B83),
    (0x0B85, 0x0B8A), (0x0B8E, 0x0B90), (0x0B92, 0x0B95), (0x0B99, 0x0B9A), (0x0B9C, 0x0B9C),
    (0x0B9E, 0x0B9F), (0x0BA3, 0x0BA4), (0x0BA8, 0x0BAA), (0x0BAE, 0x0BB9), (0x0BD0, 0x0BD0),
    (0x0C05, 0x0C0C), (0x0C0E, 0x0C10), (0x0C12, 0x0C28), (0x0C2A, 0x0C39), (0x0C3D, 0x0C3D),
    (0x0C58, 0x0C5A), (0x0C5D, 0x0C5D), (0x0C60, 0x0C61), (0x0C80, 0x0C80), (0x0C85, 0x0C8C),
    (0x0C8E, 0x0C90), (0x0C92, 0x0CA8), (0x0CAA, 0x0CB3), (0x0CB5, 0x0CB9), (0x0CBD, 0x0CBD),
    (0x0CDD, 0x0CDE), (0x0CE0, 0x0CE1), (0x0CF1, 0x0CF2), (0x0D04, 0x0D0C), (0x0D0E, 0x0D10),
    (0x0D12, 0x0D3A), (0x0D3D, 0x0D3D), (0x0D4E, 0x0D4E), (0x0D54, 0x0D56), (0x0D5F, 0x0D61),
    (0x0D7A, 0x0D7F), (0x0D85, 0x0D96), (0x0D9A, 0x0DB1), (0x0DB3, 0x0DBB), (0x0DBD, 0x0DBD),
    (0x0DC0, 0x0DC6), (0x0E01, 0x0E30), (0x0E32, 0x0E33), (0x0E40, 0x0E46), (0x0E81, 0x0E82),
    (0x0E84, 0x0E84), (0x0E86, 0x0E8A), (0x0E8C, 0x0EA3), (0x0EA5, 0x0EA5), (0x0EA7, 0x0EB0),
    (0x0EB2, 0x0EB3), (0x0EBD, 0x0EBD), (0x0EC0, 0x0EC4), (0x0EC6, 0x0EC6), (0x0EDC, 0x0EDF),
    (0x0F00, 0x0F00), (0x0F40, 0x0F47), (0x0F49, 0x0F6C), (0x0F88, 0x0F8C), (0x1000, 0x102A),
    (0x103F, 0x103F), (0x1050, 0x1055), (0x105A, 0x105D), (0x1061, 0x1061), (0x1065, 0x1066),
    (0x106E, 0x1070), (0x1075, 0x1081), (0x108E, 0x108E), (0x10A0, 0x10C5), (0x10C7, 0x10C7),
    (0x10CD, 0x10CD), (0x10D0, 0x10FA), (0x10FC, 0x1248), (0x124A, 0x124D), (0x1250, 0x1256),
    (0x1258, 0x1258), (0x125A, 0x125D), (0x1260, 0x1288), (0x128A, 0x128D), (0x1290, 0x12B0),
    (0x12B2, 0x12B5), (0x12B8, 0x12BE), (0x12C0, 0x12C0), (0x12C2, 0x12C5), (0x12C8, 0x12D6),
    (0x12D8, 0x1310), (0x1312, 0x1315), (0x1318, 0x135A), (0x1380, 0x138F), (0x13A0, 0x13F5),
    (0x13F8, 0x13FD), (0x1401, 0x166C), (0x166F, 0x167F), (0x1681, 0x169A), (0x16A0, 0x16EA),
    (0x16F1, 0x16F8), (0x1700, 0x1711), (0x171F, 0x1731), (0x1740, 0x1751), (0x1760, 0x176C),
    (0x176E, 0x1770), (0x1780, 0x17B3), (0x17D7, 0x17D7), (0x17DC, 0x17DC), (0x1820, 0x1878),
    (0x1880, 0x1884), (0x1887, 0x18A8), (0x18AA, 0x18AA), (0x18B0, 0x18F5), (0x1900, 0x191E),
    (0x1950, 0x196D), (0x1970, 0x1974), (0x1980, 0x19AB), (0x19B0, 0x19C9), (0x1A00, 0x1A16),
    (0x1A20, 0x1A54), (0x1AA7, 0x1AA7), (0x1B05, 0x1B33), (0x1B45, 0x1B4C), (0x1B83, 0x1BA0),
    (0x1BAE, 0x1BAF), (0x1BBA, 0x1BE5), (0x1C00, 0x1C23), (0x1C4D, 0x1C4F), (0x1C5A, 0x1C7D),
    (0x1C80, 0x1C8A), (0x1C90, 0x1CBA), (0x1CBD, 0x1CBF), (0x1CE9, 0x1CEC), (0x1CEE, 0x1CF3),
    (0x1CF5, 0x1CF6), (0x1CFA, 0x1CFA), (0x1D00, 0x1DBF), (0x1E00, 0x1F15), (0x1F18, 0x1F1D),
    (0x1F20, 0x1F45), (0x1F48, 0x1F4D), (0x1F50, 0x1F57), (0x1F59, 0x1F59), (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D), (0x1F5F, 0x1F7D), (0x1F80, 0x1FB4), (0x1FB6, 0x1FBC), (0x1FBE, 0x1FBE),
    (0x1FC2, 0x1FC4), (0x1FC6, 0x1FCC), (0x1FD0, 0x1FD3), (0x1FD6, 0x1FDB), (0x1FE0, 0x1FEC),
    (0x1FF2, 0x1FF4), (0x1FF6, 0x1FFC), (0x2071, 0x2071), (0x207F, 0x207F), (0x2090, 0x209C),
    (0x2102, 0x2102), (0x2107, 0x2107), (0x210A, 0x2113), (0x2115, 0x2115), (0x2119, 0x211D),
    (0x2124, 0x2124), (0x2126, 0x2126), (0x2128, 0x2128), (0x212A, 0x212D), (0x212F, 0x2139),
    (0x213C, 0x213F), (0x2145, 0x2149), (0x214E, 0x214E), (0x2183, 0x2184), (0x2C00, 0x2CE4),
    (0x2CEB, 0x2CEE), (0x2CF2, 0x2CF3), (0x2D00, 0x2D25), (0x2D27, 0x2D27), (0x2D2D, 0x2D2D),
    (0x2D30, 0x2D67), (0x2D6F, 0x2D6F), (0x2D80, 0x2D96), (0x2DA0, 0x2DA6), (0x2DA8, 0x2DAE),
    (0x2DB0, 0x2DB6), (0x2DB8, 0x2DBE), (0x2DC0, 0x2DC6), (0x2DC8, 0x2DCE), (0x2DD0, 0x2DD6),
    (0x2DD8, 0x2DDE), (0x2E2F, 0x2E2F), (0x3005, 0x3006), (0x3031, 0x3035), (0x303B, 0x303C),
    (0x3041, 0x3096), (0x309D, 0x309F), (0x30A1, 0x30FA), (0x30FC, 0x30FF), (0x3105, 0x312F),
    (0x3131, 0x318E), (0x31A0, 0x31BF), (0x31F0, 0x31FF), (0x3400, 0x4DBF), (0x4E00, 0xA48C),
    (0xA4D0, 0xA4FD), (0xA500, 0xA60C), (0xA610, 0xA61F), (0xA62A, 0xA62B), (0xA640, 0xA66E),
    (0xA67F, 0xA69D), (0xA6A0, 0xA6E5), (0xA717, 0xA71F), (0xA722, 0xA788), (0xA78B, 0xA7CD),
    (0xA7D0, 0xA7D1), (0xA7D3, 0xA7D3), (0xA7D5, 0xA7DC), (0xA7F2, 0xA801), (0xA803, 0xA805),
    (0xA807, 0xA80A), (0xA80C, 0xA822), (0xA840, 0xA873), (0xA882, 0xA8B3), (0xA8F2, 0xA8F7),
    (0xA8FB, 0xA8FB), (0xA8FD, 0xA8FE), (0xA90A, 0xA925), (0xA930, 0xA946), (0xA960, 0xA97C),
    (0xA984, 0xA9B2), (0xA9CF, 0xA9CF), (0xA9E0, 0xA9E4), (0xA9E6, 0xA9EF), (0xA9FA, 0xA9FE),
    (0xAA00, 0xAA28), (0xAA40, 0xAA42), (0xAA44, 0xAA4B), (0xAA60, 0xAA76), (0xAA7A, 0xAA7A),
    (0xAA7E, 0xAAAF), (0xAAB1, 0xAAB1), (0xAAB5, 0xAAB6), (0xAAB9, 0xAABD), (0xAAC0, 0xAAC0),
    (0xAAC2, 0xAAC2), (0xAADB, 0xAADD), (0xAAE0, 0xAAEA), (0xAAF2, 0xAAF4), (0xAB01, 0xAB06),
    (0xAB09, 0xAB0E), (0xAB11, 0xAB16), (0xAB20, 0xAB26), (0xAB28, 0xAB2E), (0xAB30, 0xAB5A),
    (0xAB5C, 0xAB69), (0xAB70, 0xABE2), (0xAC00, 0xD7A3), (0xD7B0, 0xD7C6), (0xD7CB, 0xD7FB),
    (0xF900, 0xFA6D), (0xFA70, 0xFAD9), (0xFB00, 0xFB06), (0xFB13, 0xFB17), (0xFB1D, 0xFB1D),
    (0xFB1F, 0xFB28), (0xFB2A, 0xFB36), (0xFB38, 0xFB3C), (0xFB3E, 0xFB3E), (0xFB40, 0xFB41),
    (0xFB43, 0xFB44), (0xFB46, 0xFBB1), (0xFBD3, 0xFD3D), (0xFD50, 0xFD8F), (0xFD92, 0xFDC7),
    (0xFDF0, 0xFDFB), (0xFE70, 0xFE74), (0xFE76, 0xFEFC), (0xFF21, 0xFF3A), (0xFF41, 0xFF5A),
    (0xFF66, 0xFFBE), (0xFFC2, 0xFFC7), (0xFFCA, 0xFFCF), (0xFFD2, 0xFFD7), (0xFFDA, 0xFFDC),
    (0x10000, 0x1000B), (0x1000D, 0x10026), (0x10028, 0x1003A), (0x1003C, 0x1003D),
    (0x1003F, 0x1004D), (0x10050, 0x1005D), (0x10080, 0x100FA), (0x10280, 0x1029C),
    (0x102A0, 0x102D0), (0x10300, 0x1031F), (0x1032D, 0x10340), (0x10342, 0x10349),
    (0x10350, 0x10375), (0x10380, 0x1039D), (0x103A0, 0x103C3), (0x103C8, 0x103CF),
    (0x10400, 0x1049D), (0x104B0, 0x104D3), (0x104D8, 0x104FB), (0x10500, 0x10527),
    (0x10530, 0x10563), (0x10570, 0x1057A), (0x1057C, 0x1058A), (0x1058C, 0x10592),
    (0x10594, 0x10595), (0x10597, 0x105A1), (0x105A3, 0x105B1), (0x105B3, 0x105B9),
    (0x105BB, 0x105BC), (0x105C0, 0x105F3), (0x10600, 0x10736), (0x10740, 0x10755),
    (0x10760, 0x10767), (0x10780, 0x10785), (0x10787, 0x107B0), (0x107B2, 0x107BA),
    (0x10800, 0x10805), (0x10808, 0x10808), (0x1080A, 0x10835), (0x10837, 0x10838),
    (0x1083C, 0x1083C), (0x1083F, 0x10855), (0x10860, 0x10876), (0x10880, 0x1089E),
    (0x108E0, 0x108F2), (0x108F4, 0x108F5), (0x10900, 0x10915), (0x10920, 0x10939),
    (0x10980, 0x109B7), (0x109BE, 0x109BF), (0x10A00, 0x10A00), (0x10A10, 0x10A13),
    (0x10A15, 0x10A17), (0x10A19, 0x10A35), (0x10A60, 0x10A7C), (0x10A80, 0x10A9C),
    (0x10AC0, 0x10AC7), (0x10AC9, 0x10AE4), (0x10B00, 0x10B35), (0x10B40, 0x10B55),
    (0x10B60, 0x10B72), (0x10B80, 0x10B91), (0x10C00, 0x10C48), (0x10C80, 0x10CB2),
    (0x10CC0, 0x10CF2), (0x10D00, 0x10D23), (0x10D4A, 0x10D65), (0x10D6F, 0x10D85),
    (0x10E80, 0x10EA9), (0x10EB0, 0x10EB1), (0x10EC2, 0x10EC4), (0x10F00, 0x10F1C),
    (0x10F27, 0x10F27), (0x10F30, 0x10F45), (0x10F70, 0x10F81), (0x10FB0, 0x10FC4),
    (0x10FE0, 0x10FF6), (0x11003, 0x11037), (0x11071, 0x11072), (0x11075, 0x11075),
    (0x11083, 0x110AF), (0x110D0, 0x110E8), (0x11103, 0x11126), (0x11144, 0x11144),
    (0x11147, 0x11147), (0x11150, 0x11172), (0x11176, 0x11176), (0x11183, 0x111B2),
    (0x111C1, 0x111C4), (0x111DA, 0x111DA), (0x111DC, 0x111DC), (0x11200, 0x11211),
    (0x11213, 0x1122B), (0x1123F, 0x11240), (0x11280, 0x11286), (0x11288, 0x11288),
    (0x1128A, 0x1128D), (0x1128F, 0x1129D), (0x1129F, 0x112A8), (0x112B0, 0x112DE),
    (0x11305, 0x1130C), (0x1130F, 0x11310), (0x11313, 0x11328), (0x1132A, 0x11330),
    (0x11332, 0x11333), (0x11335, 0x11339), (0x1133D, 0x1133D), (0x11350, 0x11350),
    (0x1135D, 0x11361), (0x11380, 0x11389), (0x1138B, 0x1138B), (0x1138E, 0x1138E),
    (0x11390, 0x113B5), (0x113B7, 0x113B7), (0x113D1, 0x113D1), (0x113D3, 0x113D3),
    (0x11400, 0x11434), (0x11447, 0x1144A), (0x1145F, 0x11461), (0x11480, 0x114AF),
    (0x114C4, 0x114C5), (0x114C7, 0x114C7), (0x11580, 0x115AE), (0x115D8, 0x115DB),
    (0x11600, 0x1162F), (0x11644, 0x11644), (0x11680, 0x116AA), (0x116B8, 0x116B8),
    (0x11700, 0x1171A), (0x11740, 0x11746), (0x11800, 0x1182B), (0x118A0, 0x118DF),
    (0x118FF, 0x11906), (0x11909, 0x11909), (0x1190C, 0x11913), (0x11915, 0x11916),
    (0x11918, 0x1192F), (0x1193F, 0x1193F), (0x11941, 0x11941), (0x119A0, 0x119A7),
    (0x119AA, 0x119D0), (0x119E1, 0x119E1), (0x119E3, 0x119E3), (0x11A00, 0x11A00),
    (0x11A0B, 0x11A32), (0x11A3A, 0x11A3A), (0x11A50, 0x11A50), (0x11A5C, 0x11A89),
    (0x11A9D, 0x11A9D), (0x11AB0, 0x11AF8), (0x11BC0, 0x11BE0), (0x11C00, 0x11C08),
    (0x11C0A, 0x11C2E), (0x11C40, 0x11C40), (0x11C72, 0x11C8F), (0x11D00, 0x11D06),
    (0x11D08, 0x11D09), (0x11D0B, 0x11D30), (0x11D46, 0x11D46), (0x11D60, 0x11D65),
    (0x11D67, 0x11D68), (0x11D6A, 0x11D89), (0x11D98, 0x11D98), (0x11EE0, 0x11EF2),
    (0x11F02, 0x11F02), (0x11F04, 0x11F10), (0x11F12, 0x11F33), (0x11FB0, 0x11FB0),
    (0x12000, 0x12399), (0x12480, 0x12543), (0x12F90, 0x12FF0), (0x13000, 0x1342F),
    (0x13441, 0x13446), (0x13460, 0x143FA), (0x14400, 0x14646), (0x16100, 0x1611D),
    (0x16800, 0x16A38), (0x16A40, 0x16A5E), (0x16A70, 0x16ABE), (0x16AD0, 0x16AED),
    (0x16B00, 0x16B2F), (0x16B40, 0x16B43), (0x16B63, 0x16B77), (0x16B7D, 0x16B8F),
    (0x16D40, 0x16D6C), (0x16E40, 0x16E7F), (0x16F00, 0x16F4A), (0x16F50, 0x16F50),
    (0x16F93, 0x16F9F), (0x16FE0, 0x16FE1), (0x16FE3, 0x16FE3), (0x17000, 0x187F7),
    (0x18800, 0x18CD5), (0x18CFF, 0x18D08), (0x1AFF0, 0x1AFF3), (0x1AFF5, 0x1AFFB),
    (0x1AFFD, 0x1AFFE), (0x1B000, 0x1B122), (0x1B132, 0x1B132), (0x1B150, 0x1B152),
    (0x1B155, 0x1B155), (0x1B164, 0x1B167), (0x1B170, 0x1B2FB), (0x1BC00, 0x1BC6A),
    (0x1BC70, 0x1BC7C), (0x1BC80, 0x1BC88), (0x1BC90, 0x1BC99), (0x1D400, 0x1D454),
    (0x1D456, 0x1D49C), (0x1D49E, 0x1D49F), (0x1D4A2, 0x1D4A2), (0x1D4A5, 0x1D4A6),
    (0x1D4A9, 0x1D4AC), (0x1D4AE, 0x1D4B9), (0x1D4BB, 0x1D4BB), (0x1D4BD, 0x1D4C3),
    (0x1D4C5, 0x1D505), (0x1D507, 0x1D50A), (0x1D50D, 0x1D514), (0x1D516, 0x1D51C),
    (0x1D51E, 0x1D539), (0x1D53B, 0x1D53E), (0x1D540, 0x1D544), (0x1D546, 0x1D546),
    (0x1D54A, 0x1D550), (0x1D552, 0x1D6A5), (0x1D6A8, 0x1D6C0), (0x1D6C2, 0x1D6DA),
    (0x1D6DC, 0x1D6FA), (0x1D6FC, 0x1D714), (0x1D716, 0x1D734), (0x1D736, 0x1D74E),
    (0x1D750, 0x1D76E), (0x1D770, 0x1D788), (0x1D78A, 0x1D7A8), (0x1D7AA, 0x1D7C2),
    (0x1D7C4, 0x1D7CB), (0x1DF00, 0x1DF1E), (0x1DF25, 0x1DF2A), (0x1E030, 0x1E06D),
    (0x1E100, 0x1E12C), (0x1E137, 0x1E13D), (0x1E14E, 0x1E14E), (0x1E290, 0x1E2AD),
    (0x1E2C0, 0x1E2EB), (0x1E4D0, 0x1E4EB), (0x1E5D0, 0x1E5ED), (0x1E5F0, 0x1E5F0),
    (0x1E7E0, 0x1E7E6), (0x1E7E8, 0x1E7EB), (0x1E7ED, 0x1E7EE), (0x1E7F0, 0x1E7FE),
    (0x1E800, 0x1E8C4), (0x1E900, 0x1E943), (0x1E94B, 0x1E94B), (0x1EE00, 0x1EE03),
    (0x1EE05, 0x1EE1F), (0x1EE21, 0x1EE22), (0x1EE24, 0x1EE24), (0x1EE27, 0x1EE27),
    (0x1EE29, 0x1EE32), (0x1EE34, 0x1EE37), (0x1EE39, 0x1EE39), (0x1EE3B, 0x1EE3B),
    (0x1EE42, 0x1EE42), (0x1EE47, 0x1EE47), (0x1EE49, 0x1EE49), (0x1EE4B, 0x1EE4B),
    (0x1EE4D, 0x1EE4F), (0x1EE51, 0x1EE52), (0x1EE54, 0x1EE54), (0x1EE57, 0x1EE57),
    (0x1EE59, 0x1EE59), (0x1EE5B, 0x1EE5B), (0x1EE5D, 0x1EE5D), (0x1EE5F, 0x1EE5F),
    (0x1EE61, 0x1EE62), (0x1EE64, 0x1EE64), (0x1EE67, 0x1EE6A), (0x1EE6C, 0x1EE72),
    (0x1EE74, 0x1EE77), (0x1EE79, 0x1EE7C), (0x1EE7E, 0x1EE7E), (0x1EE80, 0x1EE89),
    (0x1EE8B, 0x1EE9B), (0x1EEA1, 0x1EEA3), (0x1EEA5, 0x1EEA9), (0x1EEAB, 0x1EEBB),
    (0x20000, 0x2A6DF), (0x2A700, 0x2B739), (0x2B740, 0x2B81D), (0x2B820, 0x2CEA1),
    (0x2CEB0, 0x2EBE0), (0x2EBF0, 0x2EE5D), (0x2F800, 0x2FA1D), (0x30000, 0x3134A),
    (0x31350, 0x323AF),
];

/// `Character.isEmoji`. 150 runs, 1431 code points.
#[rustfmt::skip]
const JAVA_EMOJI_RUNS: &[(u32, u32)] = &[
    (0x0023, 0x0023), (0x002A, 0x002A), (0x0030, 0x0039), (0x00A9, 0x00A9), (0x00AE, 0x00AE),
    (0x203C, 0x203C), (0x2049, 0x2049), (0x2122, 0x2122), (0x2139, 0x2139), (0x2194, 0x2199),
    (0x21A9, 0x21AA), (0x231A, 0x231B), (0x2328, 0x2328), (0x23CF, 0x23CF), (0x23E9, 0x23F3),
    (0x23F8, 0x23FA), (0x24C2, 0x24C2), (0x25AA, 0x25AB), (0x25B6, 0x25B6), (0x25C0, 0x25C0),
    (0x25FB, 0x25FE), (0x2600, 0x2604), (0x260E, 0x260E), (0x2611, 0x2611), (0x2614, 0x2615),
    (0x2618, 0x2618), (0x261D, 0x261D), (0x2620, 0x2620), (0x2622, 0x2623), (0x2626, 0x2626),
    (0x262A, 0x262A), (0x262E, 0x262F), (0x2638, 0x263A), (0x2640, 0x2640), (0x2642, 0x2642),
    (0x2648, 0x2653), (0x265F, 0x2660), (0x2663, 0x2663), (0x2665, 0x2666), (0x2668, 0x2668),
    (0x267B, 0x267B), (0x267E, 0x267F), (0x2692, 0x2697), (0x2699, 0x2699), (0x269B, 0x269C),
    (0x26A0, 0x26A1), (0x26A7, 0x26A7), (0x26AA, 0x26AB), (0x26B0, 0x26B1), (0x26BD, 0x26BE),
    (0x26C4, 0x26C5), (0x26C8, 0x26C8), (0x26CE, 0x26CF), (0x26D1, 0x26D1), (0x26D3, 0x26D4),
    (0x26E9, 0x26EA), (0x26F0, 0x26F5), (0x26F7, 0x26FA), (0x26FD, 0x26FD), (0x2702, 0x2702),
    (0x2705, 0x2705), (0x2708, 0x270D), (0x270F, 0x270F), (0x2712, 0x2712), (0x2714, 0x2714),
    (0x2716, 0x2716), (0x271D, 0x271D), (0x2721, 0x2721), (0x2728, 0x2728), (0x2733, 0x2734),
    (0x2744, 0x2744), (0x2747, 0x2747), (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755),
    (0x2757, 0x2757), (0x2763, 0x2764), (0x2795, 0x2797), (0x27A1, 0x27A1), (0x27B0, 0x27B0),
    (0x27BF, 0x27BF), (0x2934, 0x2935), (0x2B05, 0x2B07), (0x2B1B, 0x2B1C), (0x2B50, 0x2B50),
    (0x2B55, 0x2B55), (0x3030, 0x3030), (0x303D, 0x303D), (0x3297, 0x3297), (0x3299, 0x3299),
    (0x1F004, 0x1F004), (0x1F0CF, 0x1F0CF), (0x1F170, 0x1F171), (0x1F17E, 0x1F17F),
    (0x1F18E, 0x1F18E), (0x1F191, 0x1F19A), (0x1F1E6, 0x1F1FF), (0x1F201, 0x1F202),
    (0x1F21A, 0x1F21A), (0x1F22F, 0x1F22F), (0x1F232, 0x1F23A), (0x1F250, 0x1F251),
    (0x1F300, 0x1F321), (0x1F324, 0x1F393), (0x1F396, 0x1F397), (0x1F399, 0x1F39B),
    (0x1F39E, 0x1F3F0), (0x1F3F3, 0x1F3F5), (0x1F3F7, 0x1F4FD), (0x1F4FF, 0x1F53D),
    (0x1F549, 0x1F54E), (0x1F550, 0x1F567), (0x1F56F, 0x1F570), (0x1F573, 0x1F57A),
    (0x1F587, 0x1F587), (0x1F58A, 0x1F58D), (0x1F590, 0x1F590), (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A5), (0x1F5A8, 0x1F5A8), (0x1F5B1, 0x1F5B2), (0x1F5BC, 0x1F5BC),
    (0x1F5C2, 0x1F5C4), (0x1F5D1, 0x1F5D3), (0x1F5DC, 0x1F5DE), (0x1F5E1, 0x1F5E1),
    (0x1F5E3, 0x1F5E3), (0x1F5E8, 0x1F5E8), (0x1F5EF, 0x1F5EF), (0x1F5F3, 0x1F5F3),
    (0x1F5FA, 0x1F64F), (0x1F680, 0x1F6C5), (0x1F6CB, 0x1F6D2), (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6E5), (0x1F6E9, 0x1F6E9), (0x1F6EB, 0x1F6EC), (0x1F6F0, 0x1F6F0),
    (0x1F6F3, 0x1F6FC), (0x1F7E0, 0x1F7EB), (0x1F7F0, 0x1F7F0), (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945), (0x1F947, 0x1F9FF), (0x1FA70, 0x1FA7C), (0x1FA80, 0x1FA89),
    (0x1FA8F, 0x1FAC6), (0x1FACE, 0x1FADC), (0x1FADF, 0x1FAE9), (0x1FAF0, 0x1FAF8),
];

/// `Character.isEmojiPresentation`. 80 runs, 1212 code points.
#[rustfmt::skip]
const JAVA_EMOJI_PRESENTATION_RUNS: &[(u32, u32)] = &[
    (0x231A, 0x231B), (0x23E9, 0x23EC), (0x23F0, 0x23F0), (0x23F3, 0x23F3), (0x25FD, 0x25FE),
    (0x2614, 0x2615), (0x2648, 0x2653), (0x267F, 0x267F), (0x2693, 0x2693), (0x26A1, 0x26A1),
    (0x26AA, 0x26AB), (0x26BD, 0x26BE), (0x26C4, 0x26C5), (0x26CE, 0x26CE), (0x26D4, 0x26D4),
    (0x26EA, 0x26EA), (0x26F2, 0x26F3), (0x26F5, 0x26F5), (0x26FA, 0x26FA), (0x26FD, 0x26FD),
    (0x2705, 0x2705), (0x270A, 0x270B), (0x2728, 0x2728), (0x274C, 0x274C), (0x274E, 0x274E),
    (0x2753, 0x2755), (0x2757, 0x2757), (0x2795, 0x2797), (0x27B0, 0x27B0), (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF), (0x1F18E, 0x1F18E), (0x1F191, 0x1F19A), (0x1F1E6, 0x1F1FF),
    (0x1F201, 0x1F201), (0x1F21A, 0x1F21A), (0x1F22F, 0x1F22F), (0x1F232, 0x1F236),
    (0x1F238, 0x1F23A), (0x1F250, 0x1F251), (0x1F300, 0x1F320), (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C), (0x1F37E, 0x1F393), (0x1F3A0, 0x1F3CA), (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0), (0x1F3F4, 0x1F3F4), (0x1F3F8, 0x1F43E), (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC), (0x1F4FF, 0x1F53D), (0x1F54B, 0x1F54E), (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A), (0x1F595, 0x1F596), (0x1F5A4, 0x1F5A4), (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5), (0x1F6CC, 0x1F6CC), (0x1F6D0, 0x1F6D2), (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6DF), (0x1F6EB, 0x1F6EC), (0x1F6F4, 0x1F6FC), (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0), (0x1F90C, 0x1F93A), (0x1F93C, 0x1F945), (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C), (0x1FA80, 0x1FA89), (0x1FA8F, 0x1FAC6), (0x1FACE, 0x1FADC),
    (0x1FADF, 0x1FAE9), (0x1FAF0, 0x1FAF8),
];

/// `Character.isEmojiModifier`. 1 run, 5 code points.
#[rustfmt::skip]
const JAVA_EMOJI_MODIFIER_RUNS: &[(u32, u32)] = &[(0x1F3FB, 0x1F3FF)];

/// `Character.isEmojiModifierBase`. 40 runs, 134 code points.
#[rustfmt::skip]
const JAVA_EMOJI_MODIFIER_BASE_RUNS: &[(u32, u32)] = &[
    (0x261D, 0x261D), (0x26F9, 0x26F9), (0x270A, 0x270D), (0x1F385, 0x1F385),
    (0x1F3C2, 0x1F3C4), (0x1F3C7, 0x1F3C7), (0x1F3CA, 0x1F3CC), (0x1F442, 0x1F443),
    (0x1F446, 0x1F450), (0x1F466, 0x1F478), (0x1F47C, 0x1F47C), (0x1F481, 0x1F483),
    (0x1F485, 0x1F487), (0x1F48F, 0x1F48F), (0x1F491, 0x1F491), (0x1F4AA, 0x1F4AA),
    (0x1F574, 0x1F575), (0x1F57A, 0x1F57A), (0x1F590, 0x1F590), (0x1F595, 0x1F596),
    (0x1F645, 0x1F647), (0x1F64B, 0x1F64F), (0x1F6A3, 0x1F6A3), (0x1F6B4, 0x1F6B6),
    (0x1F6C0, 0x1F6C0), (0x1F6CC, 0x1F6CC), (0x1F90C, 0x1F90C), (0x1F90F, 0x1F90F),
    (0x1F918, 0x1F91F), (0x1F926, 0x1F926), (0x1F930, 0x1F939), (0x1F93C, 0x1F93E),
    (0x1F977, 0x1F977), (0x1F9B5, 0x1F9B6), (0x1F9B8, 0x1F9B9), (0x1F9BB, 0x1F9BB),
    (0x1F9CD, 0x1F9CF), (0x1F9D1, 0x1F9DD), (0x1FAC3, 0x1FAC5), (0x1FAF0, 0x1FAF8),
];

/// `Character.isEmojiComponent`. 10 runs, 146 code points.
#[rustfmt::skip]
const JAVA_EMOJI_COMPONENT_RUNS: &[(u32, u32)] = &[
    (0x0023, 0x0023), (0x002A, 0x002A), (0x0030, 0x0039), (0x200D, 0x200D), (0x20E3, 0x20E3),
    (0xFE0F, 0xFE0F), (0x1F1E6, 0x1F1FF), (0x1F3FB, 0x1F3FF), (0x1F9B0, 0x1F9B3),
    (0xE0020, 0xE007F),
];

/// The SUPPLEMENTARY decimal digits, which `JAVA_DIGIT_RUNS` deliberately omits.
///
/// The two tables serve different callers and must not be merged.
/// `JAVA_DIGIT_RUNS` backs `Integer.parseInt` and `Character.digit(char, int)`,
/// which walk UTF-16 code UNITS — a supplementary digit reaches them as a lone
/// surrogate and correctly matches nothing (measured on JDK 25:
/// `Integer.parseInt(new String(Character.toChars(0x104A0)))` throws even though
/// `Character.digit(0x104A0, 10) == 0`). This table backs only the code-POINT
/// overloads, where the JDK really does answer for them.
/// 39 runs, 390 code points.
#[rustfmt::skip]
const JAVA_SUPPLEMENTARY_DIGIT_RUNS: &[(u32, u32)] = &[
    (0x104A0, 0x104A9), (0x10D30, 0x10D39), (0x10D40, 0x10D49), (0x11066, 0x1106F),
    (0x110F0, 0x110F9), (0x11136, 0x1113F), (0x111D0, 0x111D9), (0x112F0, 0x112F9),
    (0x11450, 0x11459), (0x114D0, 0x114D9), (0x11650, 0x11659), (0x116C0, 0x116C9),
    (0x116D0, 0x116D9), (0x116DA, 0x116E3), (0x11730, 0x11739), (0x118E0, 0x118E9),
    (0x11950, 0x11959), (0x11BF0, 0x11BF9), (0x11C50, 0x11C59), (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9), (0x11F50, 0x11F59), (0x16130, 0x16139), (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9), (0x16B50, 0x16B59), (0x16D70, 0x16D79), (0x1CCF0, 0x1CCF9),
    (0x1D7CE, 0x1D7D7), (0x1D7D8, 0x1D7E1), (0x1D7E2, 0x1D7EB), (0x1D7EC, 0x1D7F5),
    (0x1D7F6, 0x1D7FF), (0x1E140, 0x1E149), (0x1E2F0, 0x1E2F9), (0x1E4F0, 0x1E4F9),
    (0x1E5F1, 0x1E5FA), (0x1E950, 0x1E959), (0x1FBF0, 0x1FBF9),
];

/// `Character.getNumericValue(char)`, BMP only because only `(C)I` is
/// registered. 123 runs; every value increases by one across its run.
#[rustfmt::skip]
const JAVA_NUMERIC_VALUE_RUNS: &[(u32, u32, i32)] = &[
    (0x0030, 0x0039, 0), (0x0041, 0x005A, 10), (0x0061, 0x007A, 10), (0x00B2, 0x00B3, 2),
    (0x00B9, 0x00B9, 1), (0x0660, 0x0669, 0), (0x06F0, 0x06F9, 0), (0x07C0, 0x07C9, 0),
    (0x0966, 0x096F, 0), (0x09E6, 0x09EF, 0), (0x09F9, 0x09F9, 16), (0x0A66, 0x0A6F, 0),
    (0x0AE6, 0x0AEF, 0), (0x0B66, 0x0B6F, 0), (0x0BE6, 0x0BF0, 0), (0x0BF1, 0x0BF1, 100),
    (0x0BF2, 0x0BF2, 1000), (0x0C66, 0x0C6F, 0), (0x0C78, 0x0C7B, 0), (0x0C7C, 0x0C7E, 1),
    (0x0CE6, 0x0CEF, 0), (0x0D66, 0x0D70, 0), (0x0D71, 0x0D71, 100), (0x0D72, 0x0D72, 1000),
    (0x0DE6, 0x0DEF, 0), (0x0E50, 0x0E59, 0), (0x0ED0, 0x0ED9, 0), (0x0F20, 0x0F29, 0),
    (0x1040, 0x1049, 0), (0x1090, 0x1099, 0), (0x1369, 0x1372, 1), (0x1373, 0x1373, 20),
    (0x1374, 0x1374, 30), (0x1375, 0x1375, 40), (0x1376, 0x1376, 50), (0x1377, 0x1377, 60),
    (0x1378, 0x1378, 70), (0x1379, 0x1379, 80), (0x137A, 0x137A, 90), (0x137B, 0x137B, 100),
    (0x137C, 0x137C, 10000), (0x16EE, 0x16F0, 17), (0x17E0, 0x17E9, 0), (0x17F0, 0x17F9, 0),
    (0x1810, 0x1819, 0), (0x1946, 0x194F, 0), (0x19D0, 0x19D9, 0), (0x19DA, 0x19DA, 1),
    (0x1A80, 0x1A89, 0), (0x1A90, 0x1A99, 0), (0x1B50, 0x1B59, 0), (0x1BB0, 0x1BB9, 0),
    (0x1C40, 0x1C49, 0), (0x1C50, 0x1C59, 0), (0x2070, 0x2070, 0), (0x2074, 0x2079, 4),
    (0x2080, 0x2089, 0), (0x215F, 0x215F, 1), (0x2160, 0x216B, 1), (0x216C, 0x216C, 50),
    (0x216D, 0x216D, 100), (0x216E, 0x216E, 500), (0x216F, 0x216F, 1000), (0x2170, 0x217B, 1),
    (0x217C, 0x217C, 50), (0x217D, 0x217D, 100), (0x217E, 0x217E, 500),
    (0x217F, 0x217F, 1000), (0x2180, 0x2180, 1000), (0x2181, 0x2181, 5000),
    (0x2182, 0x2182, 10000), (0x2185, 0x2185, 6), (0x2186, 0x2186, 50),
    (0x2187, 0x2187, 50000), (0x2188, 0x2188, 100000), (0x2189, 0x2189, 0),
    (0x2460, 0x2473, 1), (0x2474, 0x2487, 1), (0x2488, 0x249B, 1), (0x24EA, 0x24EA, 0),
    (0x24EB, 0x24F4, 11), (0x24F5, 0x24FE, 1), (0x24FF, 0x24FF, 0), (0x2776, 0x277F, 1),
    (0x2780, 0x2789, 1), (0x278A, 0x2793, 1), (0x3007, 0x3007, 0), (0x3021, 0x3029, 1),
    (0x3038, 0x3038, 10), (0x3039, 0x3039, 20), (0x303A, 0x303A, 30), (0x3192, 0x3195, 1),
    (0x3220, 0x3229, 1), (0x3248, 0x3248, 10), (0x3249, 0x3249, 20), (0x324A, 0x324A, 30),
    (0x324B, 0x324B, 40), (0x324C, 0x324C, 50), (0x324D, 0x324D, 60), (0x324E, 0x324E, 70),
    (0x324F, 0x324F, 80), (0x3251, 0x325F, 21), (0x3280, 0x3289, 1), (0x32B1, 0x32BF, 36),
    (0xA620, 0xA629, 0), (0xA6E6, 0xA6EE, 1), (0xA6EF, 0xA6EF, 0), (0xA8D0, 0xA8D9, 0),
    (0xA900, 0xA909, 0), (0xA9D0, 0xA9D9, 0), (0xA9F0, 0xA9F9, 0), (0xAA50, 0xAA59, 0),
    (0xABF0, 0xABF9, 0), (0xF96B, 0xF96B, 3), (0xF973, 0xF973, 10), (0xF978, 0xF978, 2),
    (0xF9B2, 0xF9B2, 0), (0xF9D1, 0xF9D1, 6), (0xF9D3, 0xF9D3, 6), (0xF9FD, 0xF9FD, 10),
    (0xFF10, 0xFF19, 0), (0xFF21, 0xFF3A, 10), (0xFF41, 0xFF5A, 10),
];

/// The `-2` sentinel `getNumericValue` returns for a code point whose numeric
/// value exists but is not a non-negative integer (`U+00BD` VULGAR FRACTION ONE
/// HALF). 9 runs, 59 code points.
#[rustfmt::skip]
const JAVA_NUMERIC_VALUE_NEG2_RUNS: &[(u32, u32)] = &[
    (0x00BC, 0x00BE), (0x09F4, 0x09F8), (0x0B72, 0x0B77), (0x0D58, 0x0D5E), (0x0D73, 0x0D78),
    (0x0F2A, 0x0F33), (0x2150, 0x215E), (0x2CFD, 0x2CFD), (0xA830, 0xA835),
];

// --- Character ---

/// `Character.valueOf(char)` — JLS §5.1.7 makes the 0..127 instances CANONICAL.
///
/// The previous body allocated unconditionally, so
/// `Character.valueOf('a') == Character.valueOf('a')` was **false** on this VM
/// and **true** on HotSpot 25 — and because `javac` compiles `Character c = 'a'`
/// to exactly this call, so was `a == b` for two autoboxed ASCII chars. The
/// defect is visible to any Java code that keys on wrapper identity, not just
/// to a conformance probe.
///
/// The bound is `c <= 127`, transcribed from
/// `jdk25src/java.base/java/lang/Character.java`:
/// `if (c <= 127) { return CharacterCache.cache[(int)c]; } return new Character(c);`
/// It is NOT -128..127: `char` is unsigned, so the cache has no negative half
/// and no `+ 128` offset. Measured on HotSpot 25.0.3+9 by walking every code
/// unit — the first one for which `valueOf(c) != valueOf(c)` is **128**, and
/// U+0080, U+00FF and U+FFFF are all fresh objects there. This VM must
/// reproduce the fresh half too, so the range check has no "when in doubt,
/// cache" arm.
pub(crate) fn native_character_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if (0..=127).contains(&val) {
        let obj = cached_wrapper_box(
            ctx,
            character_cache(),
            val as usize,
            "java/lang/Character",
            Value::Int(val),
        );
        return Ok(Some(Value::Object(Some(obj))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Character");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

/// `Character.isDigit` — Unicode general category `Nd`, not `is_ascii_digit`.
///
/// W7-98(a). The old body answered ASCII-only, so every non-ASCII decimal digit
/// answered `false` where HotSpot answers `true`: measured over the whole BMP
/// against HotSpot 25, **360 of 65,536** code points disagreed (ARABIC-INDIC
/// `U+0660..U+0669`, EXTENDED ARABIC-INDIC `U+06F0..`, DEVANAGARI `U+0966..`,
/// FULLWIDTH `U+FF10..`, and 34 more runs).
///
/// [`java_char_digit`] is the fix and it was **already in this file** — a table
/// of the non-ASCII digit runs generated from JDK 25 itself by walking every
/// code point (see `JAVA_DIGIT_RUNS`) — but it had exactly ONE caller,
/// `java_parse_signed`. So `Integer.parseInt("٦٦")` answered 66 while
/// `Character.isDigit('٦')` answered false, in the same VM, from the same
/// module. Verified over all 65,536 BMP code points:
/// `java_char_digit(c, 10).is_some()` reproduces `Character.isDigit(char)` with
/// **zero** mismatches.
///
/// W7-95(C1) closes the residual the previous fix left open. This triple is
/// registered for `(I)Z` as well, and `JAVA_DIGIT_RUNS` is BMP-only by design
/// (see its doc comment), so every SUPPLEMENTARY decimal digit — `U+1D7CE`
/// MATHEMATICAL BOLD DIGIT ZERO and the 389 others — answered `false` where
/// HotSpot answers `true`. [`JAVA_SUPPLEMENTARY_DIGIT_RUNS`] is the second half,
/// and it is consulted ONLY here, never from the parse family: the two overloads
/// genuinely have different answers and merging the tables would make
/// `Integer.parseInt` more permissive than the JDK.
pub(crate) fn native_character_is_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = java_is_digit_code_point(ch);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `Character.isDigit(int)` — the BMP table plus the supplementary one.
fn java_is_digit_code_point(cp: u32) -> bool {
    char::from_u32(cp).is_some_and(|c| java_char_digit(c, 10).is_some())
        || in_code_point_runs(JAVA_SUPPLEMENTARY_DIGIT_RUNS, cp)
}

/// `Character.isLetter` — the five `L*` categories, from a JDK-25-generated
/// table.
///
/// W7-98(a) carried this as KNOWN WRONG on the grounds that Rust's std exposes
/// no `Nl` and no `Other_Alphabetic`, so `is_alphabetic` (the Unicode
/// **Alphabetic** property, `L* u Nl u Other_Alphabetic`) could not be narrowed
/// exactly. That is true of any *derivation* from Rust's tables and W7-95(C1)
/// stops trying to derive one.
///
/// The delta is enumerable from the oracle: `Character.isAlphabetic` on JDK 25
/// IS the Alphabetic property, so `isAlphabetic && !isLetter` names the whole
/// difference — **949** BMP and **1,731** total code points, all in the same
/// direction. But the census measured **957** BMP disagreements against the
/// shipping binary, and 957 != 949: Rust's Alphabetic table and JDK 25's differ
/// by eight further code points that nothing on this side can enumerate. So
/// subtracting the delta would have left a residual with no name.
/// [`JAVA_LETTER_RUNS`] is the JDK's own answer, 677 runs over every plane, and
/// leaves none.
pub(crate) fn native_character_is_letter(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = in_code_point_runs(JAVA_LETTER_RUNS, ch);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `Character.isWhitespace` — the JAVADOC rule, not `char::is_whitespace`.
///
/// W7-98(a). These are two different predicates and the difference is not an
/// approximation, it is the point of the method:
///
/// * Rust's `char::is_whitespace` is the Unicode **White_Space** property,
///   which is `Zs ∪ Zl ∪ Zp ∪ {U+0009..U+000D, U+0085}`.
/// * Java's `isWhitespace` is "a Unicode space character (`Zs`/`Zl`/`Zp`) that
///   is **not** a non-breaking space (`U+00A0`, `U+2007`, `U+202F`), **or** one
///   of `U+0009..U+000D`, `U+001C..U+001F`".
///
/// So the two disagree on eight code points, measured against HotSpot 25:
/// `U+0085` NEL, `U+00A0` NBSP, `U+2007` FIGURE SPACE and `U+202F` NARROW NBSP
/// answered `true` here and `false` on HotSpot (a non-breaking space is
/// excluded precisely *because* it must not be treated as a break
/// opportunity); the C0 file/group/record/unit separators `U+001C..U+001F`
/// answered `false` here and `true` on HotSpot. `String.isBlank`/`strip`
/// inherit every one of them — `" x".strip()` (`U+2007`) lost a character.
///
/// The derivation below is exact rather than a table: subtracting
/// `U+0009..U+000D` and `U+0085` from White_Space leaves exactly `Zs ∪ Zl ∪ Zp`
/// = Java's `isSpaceChar`, and the rest is the javadoc sentence transcribed.
/// The three excluded code points are the javadoc's own list, not a sample.
///
/// A surrogate is `Cs`, never whitespace, and falls out `false` — which is what
/// HotSpot answers.
///
/// W7-95(C1) replaced the derivation with the enumeration. The derivation was
/// CORRECT — `White_Space \ {U+0009..U+000D, U+0085}` really is `Zs u Zl u Zp`,
/// and the rest was the javadoc sentence — but it read three separate facts off
/// Rust's Unicode tables to produce an answer that is, in total, **25 code
/// points**. Enumerating them from JDK 25 is smaller, faster, provably exact,
/// and unlike the derivation it cannot silently change when the toolchain's
/// Unicode version moves. Measured on HotSpot 25 over `0..=0x10FFFF`:
/// `isWhitespace` is true for exactly `U+0009..U+000D`, `U+001C..U+0020`,
/// `U+1680`, `U+2000..U+2006`, `U+2008..U+200A`, `U+2028..U+2029`, `U+205F`,
/// `U+3000` — note the two holes, `U+2007` FIGURE SPACE and `U+00A0`/`U+202F`,
/// which are excluded precisely because a non-breaking space must not be
/// treated as a break opportunity.
pub(crate) fn native_character_is_whitespace(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = matches!(
        ch,
        0x0009..=0x000D
            | 0x001C..=0x0020
            | 0x1680
            | 0x2000..=0x2006
            | 0x2008..=0x200A
            | 0x2028..=0x2029
            | 0x205F
            | 0x3000
    );
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `Character.isUpperCase` / `isLowerCase` — the Unicode **Uppercase** and
/// **Lowercase** properties, ENUMERATED from JDK 25 instead of read off the
/// Rust toolchain's Unicode database.
///
/// The two properties are the same predicate on both sides
/// (`Lu u Other_Uppercase` / `Ll u Other_Lowercase`), so unlike `isLetter`
/// these were never a definitional mismatch — and W7-95(C1) took the BMP to
/// zero divergences by pinning six code points of toolchain-vs-JDK Unicode
/// version skew. What that sweep could not see is the **supplementary
/// planes**: it ran `0..=0xFFFF` and stopped. `0x10000..=0x10FFFF` holds 804
/// uppercase and 927 lowercase code points on JDK 25 — DESERET, OSAGE,
/// VITHKUQI, LATIN EXTENDED-F, OLD HUNGARIAN, GARAY, WARANG CITI,
/// MEDEFAIDRIN, MATHEMATICAL ALPHANUMERIC SYMBOLS, LATIN EXTENDED-G,
/// CYRILLIC EXTENDED-D, ADLAM, ENCLOSED ALPHANUMERIC SUPPLEMENT — and nothing
/// had ever compared one of them against HotSpot. GARAY (`U+10D50..U+10D85`)
/// is a Unicode 16.0 addition, i.e. precisely the kind of block where two
/// independently-versioned Unicode databases part company first, and the skew
/// the BMP sweep did find (`U+A7CE/A7CF/A7D2/A7D4/A7F1`, `U+0295`) proves the
/// two databases here are not the same version.
///
/// E7 stops deriving rather than extend the pin list into 16 more planes. A
/// `char::is_uppercase` answer is only ever as current as the compiler that
/// built this crate, and nothing in the build asserts which Unicode version
/// that is; a run table produced by executing `Character.isUpperCase` on
/// OpenJDK 25.0.3+9 over all 1,114,112 code points is JDK 25's answer by
/// construction, in every plane, and a toolchain bump cannot move it. The six
/// pinned entries and `JAVA_UNASSIGNED_ON_JDK25` are gone with the derivation
/// they patched — the table subsumes them.
///
/// Same `(first, last)` encoding and same [`in_code_point_runs`] lookup as
/// [`JAVA_LETTER_RUNS`]. 656 runs, 1,978 code points.
#[rustfmt::skip]
const JAVA_UPPERCASE_RUNS: &[(u32, u32)] = &[
    (0x0041, 0x005A), (0x00C0, 0x00D6), (0x00D8, 0x00DE), (0x0100, 0x0100), (0x0102, 0x0102),
    (0x0104, 0x0104), (0x0106, 0x0106), (0x0108, 0x0108), (0x010A, 0x010A), (0x010C, 0x010C),
    (0x010E, 0x010E), (0x0110, 0x0110), (0x0112, 0x0112), (0x0114, 0x0114), (0x0116, 0x0116),
    (0x0118, 0x0118), (0x011A, 0x011A), (0x011C, 0x011C), (0x011E, 0x011E), (0x0120, 0x0120),
    (0x0122, 0x0122), (0x0124, 0x0124), (0x0126, 0x0126), (0x0128, 0x0128), (0x012A, 0x012A),
    (0x012C, 0x012C), (0x012E, 0x012E), (0x0130, 0x0130), (0x0132, 0x0132), (0x0134, 0x0134),
    (0x0136, 0x0136), (0x0139, 0x0139), (0x013B, 0x013B), (0x013D, 0x013D), (0x013F, 0x013F),
    (0x0141, 0x0141), (0x0143, 0x0143), (0x0145, 0x0145), (0x0147, 0x0147), (0x014A, 0x014A),
    (0x014C, 0x014C), (0x014E, 0x014E), (0x0150, 0x0150), (0x0152, 0x0152), (0x0154, 0x0154),
    (0x0156, 0x0156), (0x0158, 0x0158), (0x015A, 0x015A), (0x015C, 0x015C), (0x015E, 0x015E),
    (0x0160, 0x0160), (0x0162, 0x0162), (0x0164, 0x0164), (0x0166, 0x0166), (0x0168, 0x0168),
    (0x016A, 0x016A), (0x016C, 0x016C), (0x016E, 0x016E), (0x0170, 0x0170), (0x0172, 0x0172),
    (0x0174, 0x0174), (0x0176, 0x0176), (0x0178, 0x0179), (0x017B, 0x017B), (0x017D, 0x017D),
    (0x0181, 0x0182), (0x0184, 0x0184), (0x0186, 0x0187), (0x0189, 0x018B), (0x018E, 0x0191),
    (0x0193, 0x0194), (0x0196, 0x0198), (0x019C, 0x019D), (0x019F, 0x01A0), (0x01A2, 0x01A2),
    (0x01A4, 0x01A4), (0x01A6, 0x01A7), (0x01A9, 0x01A9), (0x01AC, 0x01AC), (0x01AE, 0x01AF),
    (0x01B1, 0x01B3), (0x01B5, 0x01B5), (0x01B7, 0x01B8), (0x01BC, 0x01BC), (0x01C4, 0x01C4),
    (0x01C7, 0x01C7), (0x01CA, 0x01CA), (0x01CD, 0x01CD), (0x01CF, 0x01CF), (0x01D1, 0x01D1),
    (0x01D3, 0x01D3), (0x01D5, 0x01D5), (0x01D7, 0x01D7), (0x01D9, 0x01D9), (0x01DB, 0x01DB),
    (0x01DE, 0x01DE), (0x01E0, 0x01E0), (0x01E2, 0x01E2), (0x01E4, 0x01E4), (0x01E6, 0x01E6),
    (0x01E8, 0x01E8), (0x01EA, 0x01EA), (0x01EC, 0x01EC), (0x01EE, 0x01EE), (0x01F1, 0x01F1),
    (0x01F4, 0x01F4), (0x01F6, 0x01F8), (0x01FA, 0x01FA), (0x01FC, 0x01FC), (0x01FE, 0x01FE),
    (0x0200, 0x0200), (0x0202, 0x0202), (0x0204, 0x0204), (0x0206, 0x0206), (0x0208, 0x0208),
    (0x020A, 0x020A), (0x020C, 0x020C), (0x020E, 0x020E), (0x0210, 0x0210), (0x0212, 0x0212),
    (0x0214, 0x0214), (0x0216, 0x0216), (0x0218, 0x0218), (0x021A, 0x021A), (0x021C, 0x021C),
    (0x021E, 0x021E), (0x0220, 0x0220), (0x0222, 0x0222), (0x0224, 0x0224), (0x0226, 0x0226),
    (0x0228, 0x0228), (0x022A, 0x022A), (0x022C, 0x022C), (0x022E, 0x022E), (0x0230, 0x0230),
    (0x0232, 0x0232), (0x023A, 0x023B), (0x023D, 0x023E), (0x0241, 0x0241), (0x0243, 0x0246),
    (0x0248, 0x0248), (0x024A, 0x024A), (0x024C, 0x024C), (0x024E, 0x024E), (0x0370, 0x0370),
    (0x0372, 0x0372), (0x0376, 0x0376), (0x037F, 0x037F), (0x0386, 0x0386), (0x0388, 0x038A),
    (0x038C, 0x038C), (0x038E, 0x038F), (0x0391, 0x03A1), (0x03A3, 0x03AB), (0x03CF, 0x03CF),
    (0x03D2, 0x03D4), (0x03D8, 0x03D8), (0x03DA, 0x03DA), (0x03DC, 0x03DC), (0x03DE, 0x03DE),
    (0x03E0, 0x03E0), (0x03E2, 0x03E2), (0x03E4, 0x03E4), (0x03E6, 0x03E6), (0x03E8, 0x03E8),
    (0x03EA, 0x03EA), (0x03EC, 0x03EC), (0x03EE, 0x03EE), (0x03F4, 0x03F4), (0x03F7, 0x03F7),
    (0x03F9, 0x03FA), (0x03FD, 0x042F), (0x0460, 0x0460), (0x0462, 0x0462), (0x0464, 0x0464),
    (0x0466, 0x0466), (0x0468, 0x0468), (0x046A, 0x046A), (0x046C, 0x046C), (0x046E, 0x046E),
    (0x0470, 0x0470), (0x0472, 0x0472), (0x0474, 0x0474), (0x0476, 0x0476), (0x0478, 0x0478),
    (0x047A, 0x047A), (0x047C, 0x047C), (0x047E, 0x047E), (0x0480, 0x0480), (0x048A, 0x048A),
    (0x048C, 0x048C), (0x048E, 0x048E), (0x0490, 0x0490), (0x0492, 0x0492), (0x0494, 0x0494),
    (0x0496, 0x0496), (0x0498, 0x0498), (0x049A, 0x049A), (0x049C, 0x049C), (0x049E, 0x049E),
    (0x04A0, 0x04A0), (0x04A2, 0x04A2), (0x04A4, 0x04A4), (0x04A6, 0x04A6), (0x04A8, 0x04A8),
    (0x04AA, 0x04AA), (0x04AC, 0x04AC), (0x04AE, 0x04AE), (0x04B0, 0x04B0), (0x04B2, 0x04B2),
    (0x04B4, 0x04B4), (0x04B6, 0x04B6), (0x04B8, 0x04B8), (0x04BA, 0x04BA), (0x04BC, 0x04BC),
    (0x04BE, 0x04BE), (0x04C0, 0x04C1), (0x04C3, 0x04C3), (0x04C5, 0x04C5), (0x04C7, 0x04C7),
    (0x04C9, 0x04C9), (0x04CB, 0x04CB), (0x04CD, 0x04CD), (0x04D0, 0x04D0), (0x04D2, 0x04D2),
    (0x04D4, 0x04D4), (0x04D6, 0x04D6), (0x04D8, 0x04D8), (0x04DA, 0x04DA), (0x04DC, 0x04DC),
    (0x04DE, 0x04DE), (0x04E0, 0x04E0), (0x04E2, 0x04E2), (0x04E4, 0x04E4), (0x04E6, 0x04E6),
    (0x04E8, 0x04E8), (0x04EA, 0x04EA), (0x04EC, 0x04EC), (0x04EE, 0x04EE), (0x04F0, 0x04F0),
    (0x04F2, 0x04F2), (0x04F4, 0x04F4), (0x04F6, 0x04F6), (0x04F8, 0x04F8), (0x04FA, 0x04FA),
    (0x04FC, 0x04FC), (0x04FE, 0x04FE), (0x0500, 0x0500), (0x0502, 0x0502), (0x0504, 0x0504),
    (0x0506, 0x0506), (0x0508, 0x0508), (0x050A, 0x050A), (0x050C, 0x050C), (0x050E, 0x050E),
    (0x0510, 0x0510), (0x0512, 0x0512), (0x0514, 0x0514), (0x0516, 0x0516), (0x0518, 0x0518),
    (0x051A, 0x051A), (0x051C, 0x051C), (0x051E, 0x051E), (0x0520, 0x0520), (0x0522, 0x0522),
    (0x0524, 0x0524), (0x0526, 0x0526), (0x0528, 0x0528), (0x052A, 0x052A), (0x052C, 0x052C),
    (0x052E, 0x052E), (0x0531, 0x0556), (0x10A0, 0x10C5), (0x10C7, 0x10C7), (0x10CD, 0x10CD),
    (0x13A0, 0x13F5), (0x1C89, 0x1C89), (0x1C90, 0x1CBA), (0x1CBD, 0x1CBF), (0x1E00, 0x1E00),
    (0x1E02, 0x1E02), (0x1E04, 0x1E04), (0x1E06, 0x1E06), (0x1E08, 0x1E08), (0x1E0A, 0x1E0A),
    (0x1E0C, 0x1E0C), (0x1E0E, 0x1E0E), (0x1E10, 0x1E10), (0x1E12, 0x1E12), (0x1E14, 0x1E14),
    (0x1E16, 0x1E16), (0x1E18, 0x1E18), (0x1E1A, 0x1E1A), (0x1E1C, 0x1E1C), (0x1E1E, 0x1E1E),
    (0x1E20, 0x1E20), (0x1E22, 0x1E22), (0x1E24, 0x1E24), (0x1E26, 0x1E26), (0x1E28, 0x1E28),
    (0x1E2A, 0x1E2A), (0x1E2C, 0x1E2C), (0x1E2E, 0x1E2E), (0x1E30, 0x1E30), (0x1E32, 0x1E32),
    (0x1E34, 0x1E34), (0x1E36, 0x1E36), (0x1E38, 0x1E38), (0x1E3A, 0x1E3A), (0x1E3C, 0x1E3C),
    (0x1E3E, 0x1E3E), (0x1E40, 0x1E40), (0x1E42, 0x1E42), (0x1E44, 0x1E44), (0x1E46, 0x1E46),
    (0x1E48, 0x1E48), (0x1E4A, 0x1E4A), (0x1E4C, 0x1E4C), (0x1E4E, 0x1E4E), (0x1E50, 0x1E50),
    (0x1E52, 0x1E52), (0x1E54, 0x1E54), (0x1E56, 0x1E56), (0x1E58, 0x1E58), (0x1E5A, 0x1E5A),
    (0x1E5C, 0x1E5C), (0x1E5E, 0x1E5E), (0x1E60, 0x1E60), (0x1E62, 0x1E62), (0x1E64, 0x1E64),
    (0x1E66, 0x1E66), (0x1E68, 0x1E68), (0x1E6A, 0x1E6A), (0x1E6C, 0x1E6C), (0x1E6E, 0x1E6E),
    (0x1E70, 0x1E70), (0x1E72, 0x1E72), (0x1E74, 0x1E74), (0x1E76, 0x1E76), (0x1E78, 0x1E78),
    (0x1E7A, 0x1E7A), (0x1E7C, 0x1E7C), (0x1E7E, 0x1E7E), (0x1E80, 0x1E80), (0x1E82, 0x1E82),
    (0x1E84, 0x1E84), (0x1E86, 0x1E86), (0x1E88, 0x1E88), (0x1E8A, 0x1E8A), (0x1E8C, 0x1E8C),
    (0x1E8E, 0x1E8E), (0x1E90, 0x1E90), (0x1E92, 0x1E92), (0x1E94, 0x1E94), (0x1E9E, 0x1E9E),
    (0x1EA0, 0x1EA0), (0x1EA2, 0x1EA2), (0x1EA4, 0x1EA4), (0x1EA6, 0x1EA6), (0x1EA8, 0x1EA8),
    (0x1EAA, 0x1EAA), (0x1EAC, 0x1EAC), (0x1EAE, 0x1EAE), (0x1EB0, 0x1EB0), (0x1EB2, 0x1EB2),
    (0x1EB4, 0x1EB4), (0x1EB6, 0x1EB6), (0x1EB8, 0x1EB8), (0x1EBA, 0x1EBA), (0x1EBC, 0x1EBC),
    (0x1EBE, 0x1EBE), (0x1EC0, 0x1EC0), (0x1EC2, 0x1EC2), (0x1EC4, 0x1EC4), (0x1EC6, 0x1EC6),
    (0x1EC8, 0x1EC8), (0x1ECA, 0x1ECA), (0x1ECC, 0x1ECC), (0x1ECE, 0x1ECE), (0x1ED0, 0x1ED0),
    (0x1ED2, 0x1ED2), (0x1ED4, 0x1ED4), (0x1ED6, 0x1ED6), (0x1ED8, 0x1ED8), (0x1EDA, 0x1EDA),
    (0x1EDC, 0x1EDC), (0x1EDE, 0x1EDE), (0x1EE0, 0x1EE0), (0x1EE2, 0x1EE2), (0x1EE4, 0x1EE4),
    (0x1EE6, 0x1EE6), (0x1EE8, 0x1EE8), (0x1EEA, 0x1EEA), (0x1EEC, 0x1EEC), (0x1EEE, 0x1EEE),
    (0x1EF0, 0x1EF0), (0x1EF2, 0x1EF2), (0x1EF4, 0x1EF4), (0x1EF6, 0x1EF6), (0x1EF8, 0x1EF8),
    (0x1EFA, 0x1EFA), (0x1EFC, 0x1EFC), (0x1EFE, 0x1EFE), (0x1F08, 0x1F0F), (0x1F18, 0x1F1D),
    (0x1F28, 0x1F2F), (0x1F38, 0x1F3F), (0x1F48, 0x1F4D), (0x1F59, 0x1F59), (0x1F5B, 0x1F5B),
    (0x1F5D, 0x1F5D), (0x1F5F, 0x1F5F), (0x1F68, 0x1F6F), (0x1FB8, 0x1FBB), (0x1FC8, 0x1FCB),
    (0x1FD8, 0x1FDB), (0x1FE8, 0x1FEC), (0x1FF8, 0x1FFB), (0x2102, 0x2102), (0x2107, 0x2107),
    (0x210B, 0x210D), (0x2110, 0x2112), (0x2115, 0x2115), (0x2119, 0x211D), (0x2124, 0x2124),
    (0x2126, 0x2126), (0x2128, 0x2128), (0x212A, 0x212D), (0x2130, 0x2133), (0x213E, 0x213F),
    (0x2145, 0x2145), (0x2160, 0x216F), (0x2183, 0x2183), (0x24B6, 0x24CF), (0x2C00, 0x2C2F),
    (0x2C60, 0x2C60), (0x2C62, 0x2C64), (0x2C67, 0x2C67), (0x2C69, 0x2C69), (0x2C6B, 0x2C6B),
    (0x2C6D, 0x2C70), (0x2C72, 0x2C72), (0x2C75, 0x2C75), (0x2C7E, 0x2C80), (0x2C82, 0x2C82),
    (0x2C84, 0x2C84), (0x2C86, 0x2C86), (0x2C88, 0x2C88), (0x2C8A, 0x2C8A), (0x2C8C, 0x2C8C),
    (0x2C8E, 0x2C8E), (0x2C90, 0x2C90), (0x2C92, 0x2C92), (0x2C94, 0x2C94), (0x2C96, 0x2C96),
    (0x2C98, 0x2C98), (0x2C9A, 0x2C9A), (0x2C9C, 0x2C9C), (0x2C9E, 0x2C9E), (0x2CA0, 0x2CA0),
    (0x2CA2, 0x2CA2), (0x2CA4, 0x2CA4), (0x2CA6, 0x2CA6), (0x2CA8, 0x2CA8), (0x2CAA, 0x2CAA),
    (0x2CAC, 0x2CAC), (0x2CAE, 0x2CAE), (0x2CB0, 0x2CB0), (0x2CB2, 0x2CB2), (0x2CB4, 0x2CB4),
    (0x2CB6, 0x2CB6), (0x2CB8, 0x2CB8), (0x2CBA, 0x2CBA), (0x2CBC, 0x2CBC), (0x2CBE, 0x2CBE),
    (0x2CC0, 0x2CC0), (0x2CC2, 0x2CC2), (0x2CC4, 0x2CC4), (0x2CC6, 0x2CC6), (0x2CC8, 0x2CC8),
    (0x2CCA, 0x2CCA), (0x2CCC, 0x2CCC), (0x2CCE, 0x2CCE), (0x2CD0, 0x2CD0), (0x2CD2, 0x2CD2),
    (0x2CD4, 0x2CD4), (0x2CD6, 0x2CD6), (0x2CD8, 0x2CD8), (0x2CDA, 0x2CDA), (0x2CDC, 0x2CDC),
    (0x2CDE, 0x2CDE), (0x2CE0, 0x2CE0), (0x2CE2, 0x2CE2), (0x2CEB, 0x2CEB), (0x2CED, 0x2CED),
    (0x2CF2, 0x2CF2), (0xA640, 0xA640), (0xA642, 0xA642), (0xA644, 0xA644), (0xA646, 0xA646),
    (0xA648, 0xA648), (0xA64A, 0xA64A), (0xA64C, 0xA64C), (0xA64E, 0xA64E), (0xA650, 0xA650),
    (0xA652, 0xA652), (0xA654, 0xA654), (0xA656, 0xA656), (0xA658, 0xA658), (0xA65A, 0xA65A),
    (0xA65C, 0xA65C), (0xA65E, 0xA65E), (0xA660, 0xA660), (0xA662, 0xA662), (0xA664, 0xA664),
    (0xA666, 0xA666), (0xA668, 0xA668), (0xA66A, 0xA66A), (0xA66C, 0xA66C), (0xA680, 0xA680),
    (0xA682, 0xA682), (0xA684, 0xA684), (0xA686, 0xA686), (0xA688, 0xA688), (0xA68A, 0xA68A),
    (0xA68C, 0xA68C), (0xA68E, 0xA68E), (0xA690, 0xA690), (0xA692, 0xA692), (0xA694, 0xA694),
    (0xA696, 0xA696), (0xA698, 0xA698), (0xA69A, 0xA69A), (0xA722, 0xA722), (0xA724, 0xA724),
    (0xA726, 0xA726), (0xA728, 0xA728), (0xA72A, 0xA72A), (0xA72C, 0xA72C), (0xA72E, 0xA72E),
    (0xA732, 0xA732), (0xA734, 0xA734), (0xA736, 0xA736), (0xA738, 0xA738), (0xA73A, 0xA73A),
    (0xA73C, 0xA73C), (0xA73E, 0xA73E), (0xA740, 0xA740), (0xA742, 0xA742), (0xA744, 0xA744),
    (0xA746, 0xA746), (0xA748, 0xA748), (0xA74A, 0xA74A), (0xA74C, 0xA74C), (0xA74E, 0xA74E),
    (0xA750, 0xA750), (0xA752, 0xA752), (0xA754, 0xA754), (0xA756, 0xA756), (0xA758, 0xA758),
    (0xA75A, 0xA75A), (0xA75C, 0xA75C), (0xA75E, 0xA75E), (0xA760, 0xA760), (0xA762, 0xA762),
    (0xA764, 0xA764), (0xA766, 0xA766), (0xA768, 0xA768), (0xA76A, 0xA76A), (0xA76C, 0xA76C),
    (0xA76E, 0xA76E), (0xA779, 0xA779), (0xA77B, 0xA77B), (0xA77D, 0xA77E), (0xA780, 0xA780),
    (0xA782, 0xA782), (0xA784, 0xA784), (0xA786, 0xA786), (0xA78B, 0xA78B), (0xA78D, 0xA78D),
    (0xA790, 0xA790), (0xA792, 0xA792), (0xA796, 0xA796), (0xA798, 0xA798), (0xA79A, 0xA79A),
    (0xA79C, 0xA79C), (0xA79E, 0xA79E), (0xA7A0, 0xA7A0), (0xA7A2, 0xA7A2), (0xA7A4, 0xA7A4),
    (0xA7A6, 0xA7A6), (0xA7A8, 0xA7A8), (0xA7AA, 0xA7AE), (0xA7B0, 0xA7B4), (0xA7B6, 0xA7B6),
    (0xA7B8, 0xA7B8), (0xA7BA, 0xA7BA), (0xA7BC, 0xA7BC), (0xA7BE, 0xA7BE), (0xA7C0, 0xA7C0),
    (0xA7C2, 0xA7C2), (0xA7C4, 0xA7C7), (0xA7C9, 0xA7C9), (0xA7CB, 0xA7CC), (0xA7D0, 0xA7D0),
    (0xA7D6, 0xA7D6), (0xA7D8, 0xA7D8), (0xA7DA, 0xA7DA), (0xA7DC, 0xA7DC), (0xA7F5, 0xA7F5),
    (0xFF21, 0xFF3A), (0x10400, 0x10427), (0x104B0, 0x104D3), (0x10570, 0x1057A), (0x1057C, 0x1058A),
    (0x1058C, 0x10592), (0x10594, 0x10595), (0x10C80, 0x10CB2), (0x10D50, 0x10D65), (0x118A0, 0x118BF),
    (0x16E40, 0x16E5F), (0x1D400, 0x1D419), (0x1D434, 0x1D44D), (0x1D468, 0x1D481), (0x1D49C, 0x1D49C),
    (0x1D49E, 0x1D49F), (0x1D4A2, 0x1D4A2), (0x1D4A5, 0x1D4A6), (0x1D4A9, 0x1D4AC), (0x1D4AE, 0x1D4B5),
    (0x1D4D0, 0x1D4E9), (0x1D504, 0x1D505), (0x1D507, 0x1D50A), (0x1D50D, 0x1D514), (0x1D516, 0x1D51C),
    (0x1D538, 0x1D539), (0x1D53B, 0x1D53E), (0x1D540, 0x1D544), (0x1D546, 0x1D546), (0x1D54A, 0x1D550),
    (0x1D56C, 0x1D585), (0x1D5A0, 0x1D5B9), (0x1D5D4, 0x1D5ED), (0x1D608, 0x1D621), (0x1D63C, 0x1D655),
    (0x1D670, 0x1D689), (0x1D6A8, 0x1D6C0), (0x1D6E2, 0x1D6FA), (0x1D71C, 0x1D734), (0x1D756, 0x1D76E),
    (0x1D790, 0x1D7A8), (0x1D7CA, 0x1D7CA), (0x1E900, 0x1E921), (0x1F130, 0x1F149), (0x1F150, 0x1F169),
    (0x1F170, 0x1F189),
];

/// `Character.isLowerCase`. See [`JAVA_UPPERCASE_RUNS`] — same provenance,
/// same sweep, same encoding. 675 runs, 2,569 code points.
#[rustfmt::skip]
const JAVA_LOWERCASE_RUNS: &[(u32, u32)] = &[
    (0x0061, 0x007A), (0x00AA, 0x00AA), (0x00B5, 0x00B5), (0x00BA, 0x00BA), (0x00DF, 0x00F6),
    (0x00F8, 0x00FF), (0x0101, 0x0101), (0x0103, 0x0103), (0x0105, 0x0105), (0x0107, 0x0107),
    (0x0109, 0x0109), (0x010B, 0x010B), (0x010D, 0x010D), (0x010F, 0x010F), (0x0111, 0x0111),
    (0x0113, 0x0113), (0x0115, 0x0115), (0x0117, 0x0117), (0x0119, 0x0119), (0x011B, 0x011B),
    (0x011D, 0x011D), (0x011F, 0x011F), (0x0121, 0x0121), (0x0123, 0x0123), (0x0125, 0x0125),
    (0x0127, 0x0127), (0x0129, 0x0129), (0x012B, 0x012B), (0x012D, 0x012D), (0x012F, 0x012F),
    (0x0131, 0x0131), (0x0133, 0x0133), (0x0135, 0x0135), (0x0137, 0x0138), (0x013A, 0x013A),
    (0x013C, 0x013C), (0x013E, 0x013E), (0x0140, 0x0140), (0x0142, 0x0142), (0x0144, 0x0144),
    (0x0146, 0x0146), (0x0148, 0x0149), (0x014B, 0x014B), (0x014D, 0x014D), (0x014F, 0x014F),
    (0x0151, 0x0151), (0x0153, 0x0153), (0x0155, 0x0155), (0x0157, 0x0157), (0x0159, 0x0159),
    (0x015B, 0x015B), (0x015D, 0x015D), (0x015F, 0x015F), (0x0161, 0x0161), (0x0163, 0x0163),
    (0x0165, 0x0165), (0x0167, 0x0167), (0x0169, 0x0169), (0x016B, 0x016B), (0x016D, 0x016D),
    (0x016F, 0x016F), (0x0171, 0x0171), (0x0173, 0x0173), (0x0175, 0x0175), (0x0177, 0x0177),
    (0x017A, 0x017A), (0x017C, 0x017C), (0x017E, 0x0180), (0x0183, 0x0183), (0x0185, 0x0185),
    (0x0188, 0x0188), (0x018C, 0x018D), (0x0192, 0x0192), (0x0195, 0x0195), (0x0199, 0x019B),
    (0x019E, 0x019E), (0x01A1, 0x01A1), (0x01A3, 0x01A3), (0x01A5, 0x01A5), (0x01A8, 0x01A8),
    (0x01AA, 0x01AB), (0x01AD, 0x01AD), (0x01B0, 0x01B0), (0x01B4, 0x01B4), (0x01B6, 0x01B6),
    (0x01B9, 0x01BA), (0x01BD, 0x01BF), (0x01C6, 0x01C6), (0x01C9, 0x01C9), (0x01CC, 0x01CC),
    (0x01CE, 0x01CE), (0x01D0, 0x01D0), (0x01D2, 0x01D2), (0x01D4, 0x01D4), (0x01D6, 0x01D6),
    (0x01D8, 0x01D8), (0x01DA, 0x01DA), (0x01DC, 0x01DD), (0x01DF, 0x01DF), (0x01E1, 0x01E1),
    (0x01E3, 0x01E3), (0x01E5, 0x01E5), (0x01E7, 0x01E7), (0x01E9, 0x01E9), (0x01EB, 0x01EB),
    (0x01ED, 0x01ED), (0x01EF, 0x01F0), (0x01F3, 0x01F3), (0x01F5, 0x01F5), (0x01F9, 0x01F9),
    (0x01FB, 0x01FB), (0x01FD, 0x01FD), (0x01FF, 0x01FF), (0x0201, 0x0201), (0x0203, 0x0203),
    (0x0205, 0x0205), (0x0207, 0x0207), (0x0209, 0x0209), (0x020B, 0x020B), (0x020D, 0x020D),
    (0x020F, 0x020F), (0x0211, 0x0211), (0x0213, 0x0213), (0x0215, 0x0215), (0x0217, 0x0217),
    (0x0219, 0x0219), (0x021B, 0x021B), (0x021D, 0x021D), (0x021F, 0x021F), (0x0221, 0x0221),
    (0x0223, 0x0223), (0x0225, 0x0225), (0x0227, 0x0227), (0x0229, 0x0229), (0x022B, 0x022B),
    (0x022D, 0x022D), (0x022F, 0x022F), (0x0231, 0x0231), (0x0233, 0x0239), (0x023C, 0x023C),
    (0x023F, 0x0240), (0x0242, 0x0242), (0x0247, 0x0247), (0x0249, 0x0249), (0x024B, 0x024B),
    (0x024D, 0x024D), (0x024F, 0x0293), (0x0295, 0x02B8), (0x02C0, 0x02C1), (0x02E0, 0x02E4),
    (0x0345, 0x0345), (0x0371, 0x0371), (0x0373, 0x0373), (0x0377, 0x0377), (0x037A, 0x037D),
    (0x0390, 0x0390), (0x03AC, 0x03CE), (0x03D0, 0x03D1), (0x03D5, 0x03D7), (0x03D9, 0x03D9),
    (0x03DB, 0x03DB), (0x03DD, 0x03DD), (0x03DF, 0x03DF), (0x03E1, 0x03E1), (0x03E3, 0x03E3),
    (0x03E5, 0x03E5), (0x03E7, 0x03E7), (0x03E9, 0x03E9), (0x03EB, 0x03EB), (0x03ED, 0x03ED),
    (0x03EF, 0x03F3), (0x03F5, 0x03F5), (0x03F8, 0x03F8), (0x03FB, 0x03FC), (0x0430, 0x045F),
    (0x0461, 0x0461), (0x0463, 0x0463), (0x0465, 0x0465), (0x0467, 0x0467), (0x0469, 0x0469),
    (0x046B, 0x046B), (0x046D, 0x046D), (0x046F, 0x046F), (0x0471, 0x0471), (0x0473, 0x0473),
    (0x0475, 0x0475), (0x0477, 0x0477), (0x0479, 0x0479), (0x047B, 0x047B), (0x047D, 0x047D),
    (0x047F, 0x047F), (0x0481, 0x0481), (0x048B, 0x048B), (0x048D, 0x048D), (0x048F, 0x048F),
    (0x0491, 0x0491), (0x0493, 0x0493), (0x0495, 0x0495), (0x0497, 0x0497), (0x0499, 0x0499),
    (0x049B, 0x049B), (0x049D, 0x049D), (0x049F, 0x049F), (0x04A1, 0x04A1), (0x04A3, 0x04A3),
    (0x04A5, 0x04A5), (0x04A7, 0x04A7), (0x04A9, 0x04A9), (0x04AB, 0x04AB), (0x04AD, 0x04AD),
    (0x04AF, 0x04AF), (0x04B1, 0x04B1), (0x04B3, 0x04B3), (0x04B5, 0x04B5), (0x04B7, 0x04B7),
    (0x04B9, 0x04B9), (0x04BB, 0x04BB), (0x04BD, 0x04BD), (0x04BF, 0x04BF), (0x04C2, 0x04C2),
    (0x04C4, 0x04C4), (0x04C6, 0x04C6), (0x04C8, 0x04C8), (0x04CA, 0x04CA), (0x04CC, 0x04CC),
    (0x04CE, 0x04CF), (0x04D1, 0x04D1), (0x04D3, 0x04D3), (0x04D5, 0x04D5), (0x04D7, 0x04D7),
    (0x04D9, 0x04D9), (0x04DB, 0x04DB), (0x04DD, 0x04DD), (0x04DF, 0x04DF), (0x04E1, 0x04E1),
    (0x04E3, 0x04E3), (0x04E5, 0x04E5), (0x04E7, 0x04E7), (0x04E9, 0x04E9), (0x04EB, 0x04EB),
    (0x04ED, 0x04ED), (0x04EF, 0x04EF), (0x04F1, 0x04F1), (0x04F3, 0x04F3), (0x04F5, 0x04F5),
    (0x04F7, 0x04F7), (0x04F9, 0x04F9), (0x04FB, 0x04FB), (0x04FD, 0x04FD), (0x04FF, 0x04FF),
    (0x0501, 0x0501), (0x0503, 0x0503), (0x0505, 0x0505), (0x0507, 0x0507), (0x0509, 0x0509),
    (0x050B, 0x050B), (0x050D, 0x050D), (0x050F, 0x050F), (0x0511, 0x0511), (0x0513, 0x0513),
    (0x0515, 0x0515), (0x0517, 0x0517), (0x0519, 0x0519), (0x051B, 0x051B), (0x051D, 0x051D),
    (0x051F, 0x051F), (0x0521, 0x0521), (0x0523, 0x0523), (0x0525, 0x0525), (0x0527, 0x0527),
    (0x0529, 0x0529), (0x052B, 0x052B), (0x052D, 0x052D), (0x052F, 0x052F), (0x0560, 0x0588),
    (0x10D0, 0x10FA), (0x10FC, 0x10FF), (0x13F8, 0x13FD), (0x1C80, 0x1C88), (0x1C8A, 0x1C8A),
    (0x1D00, 0x1DBF), (0x1E01, 0x1E01), (0x1E03, 0x1E03), (0x1E05, 0x1E05), (0x1E07, 0x1E07),
    (0x1E09, 0x1E09), (0x1E0B, 0x1E0B), (0x1E0D, 0x1E0D), (0x1E0F, 0x1E0F), (0x1E11, 0x1E11),
    (0x1E13, 0x1E13), (0x1E15, 0x1E15), (0x1E17, 0x1E17), (0x1E19, 0x1E19), (0x1E1B, 0x1E1B),
    (0x1E1D, 0x1E1D), (0x1E1F, 0x1E1F), (0x1E21, 0x1E21), (0x1E23, 0x1E23), (0x1E25, 0x1E25),
    (0x1E27, 0x1E27), (0x1E29, 0x1E29), (0x1E2B, 0x1E2B), (0x1E2D, 0x1E2D), (0x1E2F, 0x1E2F),
    (0x1E31, 0x1E31), (0x1E33, 0x1E33), (0x1E35, 0x1E35), (0x1E37, 0x1E37), (0x1E39, 0x1E39),
    (0x1E3B, 0x1E3B), (0x1E3D, 0x1E3D), (0x1E3F, 0x1E3F), (0x1E41, 0x1E41), (0x1E43, 0x1E43),
    (0x1E45, 0x1E45), (0x1E47, 0x1E47), (0x1E49, 0x1E49), (0x1E4B, 0x1E4B), (0x1E4D, 0x1E4D),
    (0x1E4F, 0x1E4F), (0x1E51, 0x1E51), (0x1E53, 0x1E53), (0x1E55, 0x1E55), (0x1E57, 0x1E57),
    (0x1E59, 0x1E59), (0x1E5B, 0x1E5B), (0x1E5D, 0x1E5D), (0x1E5F, 0x1E5F), (0x1E61, 0x1E61),
    (0x1E63, 0x1E63), (0x1E65, 0x1E65), (0x1E67, 0x1E67), (0x1E69, 0x1E69), (0x1E6B, 0x1E6B),
    (0x1E6D, 0x1E6D), (0x1E6F, 0x1E6F), (0x1E71, 0x1E71), (0x1E73, 0x1E73), (0x1E75, 0x1E75),
    (0x1E77, 0x1E77), (0x1E79, 0x1E79), (0x1E7B, 0x1E7B), (0x1E7D, 0x1E7D), (0x1E7F, 0x1E7F),
    (0x1E81, 0x1E81), (0x1E83, 0x1E83), (0x1E85, 0x1E85), (0x1E87, 0x1E87), (0x1E89, 0x1E89),
    (0x1E8B, 0x1E8B), (0x1E8D, 0x1E8D), (0x1E8F, 0x1E8F), (0x1E91, 0x1E91), (0x1E93, 0x1E93),
    (0x1E95, 0x1E9D), (0x1E9F, 0x1E9F), (0x1EA1, 0x1EA1), (0x1EA3, 0x1EA3), (0x1EA5, 0x1EA5),
    (0x1EA7, 0x1EA7), (0x1EA9, 0x1EA9), (0x1EAB, 0x1EAB), (0x1EAD, 0x1EAD), (0x1EAF, 0x1EAF),
    (0x1EB1, 0x1EB1), (0x1EB3, 0x1EB3), (0x1EB5, 0x1EB5), (0x1EB7, 0x1EB7), (0x1EB9, 0x1EB9),
    (0x1EBB, 0x1EBB), (0x1EBD, 0x1EBD), (0x1EBF, 0x1EBF), (0x1EC1, 0x1EC1), (0x1EC3, 0x1EC3),
    (0x1EC5, 0x1EC5), (0x1EC7, 0x1EC7), (0x1EC9, 0x1EC9), (0x1ECB, 0x1ECB), (0x1ECD, 0x1ECD),
    (0x1ECF, 0x1ECF), (0x1ED1, 0x1ED1), (0x1ED3, 0x1ED3), (0x1ED5, 0x1ED5), (0x1ED7, 0x1ED7),
    (0x1ED9, 0x1ED9), (0x1EDB, 0x1EDB), (0x1EDD, 0x1EDD), (0x1EDF, 0x1EDF), (0x1EE1, 0x1EE1),
    (0x1EE3, 0x1EE3), (0x1EE5, 0x1EE5), (0x1EE7, 0x1EE7), (0x1EE9, 0x1EE9), (0x1EEB, 0x1EEB),
    (0x1EED, 0x1EED), (0x1EEF, 0x1EEF), (0x1EF1, 0x1EF1), (0x1EF3, 0x1EF3), (0x1EF5, 0x1EF5),
    (0x1EF7, 0x1EF7), (0x1EF9, 0x1EF9), (0x1EFB, 0x1EFB), (0x1EFD, 0x1EFD), (0x1EFF, 0x1F07),
    (0x1F10, 0x1F15), (0x1F20, 0x1F27), (0x1F30, 0x1F37), (0x1F40, 0x1F45), (0x1F50, 0x1F57),
    (0x1F60, 0x1F67), (0x1F70, 0x1F7D), (0x1F80, 0x1F87), (0x1F90, 0x1F97), (0x1FA0, 0x1FA7),
    (0x1FB0, 0x1FB4), (0x1FB6, 0x1FB7), (0x1FBE, 0x1FBE), (0x1FC2, 0x1FC4), (0x1FC6, 0x1FC7),
    (0x1FD0, 0x1FD3), (0x1FD6, 0x1FD7), (0x1FE0, 0x1FE7), (0x1FF2, 0x1FF4), (0x1FF6, 0x1FF7),
    (0x2071, 0x2071), (0x207F, 0x207F), (0x2090, 0x209C), (0x210A, 0x210A), (0x210E, 0x210F),
    (0x2113, 0x2113), (0x212F, 0x212F), (0x2134, 0x2134), (0x2139, 0x2139), (0x213C, 0x213D),
    (0x2146, 0x2149), (0x214E, 0x214E), (0x2170, 0x217F), (0x2184, 0x2184), (0x24D0, 0x24E9),
    (0x2C30, 0x2C5F), (0x2C61, 0x2C61), (0x2C65, 0x2C66), (0x2C68, 0x2C68), (0x2C6A, 0x2C6A),
    (0x2C6C, 0x2C6C), (0x2C71, 0x2C71), (0x2C73, 0x2C74), (0x2C76, 0x2C7D), (0x2C81, 0x2C81),
    (0x2C83, 0x2C83), (0x2C85, 0x2C85), (0x2C87, 0x2C87), (0x2C89, 0x2C89), (0x2C8B, 0x2C8B),
    (0x2C8D, 0x2C8D), (0x2C8F, 0x2C8F), (0x2C91, 0x2C91), (0x2C93, 0x2C93), (0x2C95, 0x2C95),
    (0x2C97, 0x2C97), (0x2C99, 0x2C99), (0x2C9B, 0x2C9B), (0x2C9D, 0x2C9D), (0x2C9F, 0x2C9F),
    (0x2CA1, 0x2CA1), (0x2CA3, 0x2CA3), (0x2CA5, 0x2CA5), (0x2CA7, 0x2CA7), (0x2CA9, 0x2CA9),
    (0x2CAB, 0x2CAB), (0x2CAD, 0x2CAD), (0x2CAF, 0x2CAF), (0x2CB1, 0x2CB1), (0x2CB3, 0x2CB3),
    (0x2CB5, 0x2CB5), (0x2CB7, 0x2CB7), (0x2CB9, 0x2CB9), (0x2CBB, 0x2CBB), (0x2CBD, 0x2CBD),
    (0x2CBF, 0x2CBF), (0x2CC1, 0x2CC1), (0x2CC3, 0x2CC3), (0x2CC5, 0x2CC5), (0x2CC7, 0x2CC7),
    (0x2CC9, 0x2CC9), (0x2CCB, 0x2CCB), (0x2CCD, 0x2CCD), (0x2CCF, 0x2CCF), (0x2CD1, 0x2CD1),
    (0x2CD3, 0x2CD3), (0x2CD5, 0x2CD5), (0x2CD7, 0x2CD7), (0x2CD9, 0x2CD9), (0x2CDB, 0x2CDB),
    (0x2CDD, 0x2CDD), (0x2CDF, 0x2CDF), (0x2CE1, 0x2CE1), (0x2CE3, 0x2CE4), (0x2CEC, 0x2CEC),
    (0x2CEE, 0x2CEE), (0x2CF3, 0x2CF3), (0x2D00, 0x2D25), (0x2D27, 0x2D27), (0x2D2D, 0x2D2D),
    (0xA641, 0xA641), (0xA643, 0xA643), (0xA645, 0xA645), (0xA647, 0xA647), (0xA649, 0xA649),
    (0xA64B, 0xA64B), (0xA64D, 0xA64D), (0xA64F, 0xA64F), (0xA651, 0xA651), (0xA653, 0xA653),
    (0xA655, 0xA655), (0xA657, 0xA657), (0xA659, 0xA659), (0xA65B, 0xA65B), (0xA65D, 0xA65D),
    (0xA65F, 0xA65F), (0xA661, 0xA661), (0xA663, 0xA663), (0xA665, 0xA665), (0xA667, 0xA667),
    (0xA669, 0xA669), (0xA66B, 0xA66B), (0xA66D, 0xA66D), (0xA681, 0xA681), (0xA683, 0xA683),
    (0xA685, 0xA685), (0xA687, 0xA687), (0xA689, 0xA689), (0xA68B, 0xA68B), (0xA68D, 0xA68D),
    (0xA68F, 0xA68F), (0xA691, 0xA691), (0xA693, 0xA693), (0xA695, 0xA695), (0xA697, 0xA697),
    (0xA699, 0xA699), (0xA69B, 0xA69D), (0xA723, 0xA723), (0xA725, 0xA725), (0xA727, 0xA727),
    (0xA729, 0xA729), (0xA72B, 0xA72B), (0xA72D, 0xA72D), (0xA72F, 0xA731), (0xA733, 0xA733),
    (0xA735, 0xA735), (0xA737, 0xA737), (0xA739, 0xA739), (0xA73B, 0xA73B), (0xA73D, 0xA73D),
    (0xA73F, 0xA73F), (0xA741, 0xA741), (0xA743, 0xA743), (0xA745, 0xA745), (0xA747, 0xA747),
    (0xA749, 0xA749), (0xA74B, 0xA74B), (0xA74D, 0xA74D), (0xA74F, 0xA74F), (0xA751, 0xA751),
    (0xA753, 0xA753), (0xA755, 0xA755), (0xA757, 0xA757), (0xA759, 0xA759), (0xA75B, 0xA75B),
    (0xA75D, 0xA75D), (0xA75F, 0xA75F), (0xA761, 0xA761), (0xA763, 0xA763), (0xA765, 0xA765),
    (0xA767, 0xA767), (0xA769, 0xA769), (0xA76B, 0xA76B), (0xA76D, 0xA76D), (0xA76F, 0xA778),
    (0xA77A, 0xA77A), (0xA77C, 0xA77C), (0xA77F, 0xA77F), (0xA781, 0xA781), (0xA783, 0xA783),
    (0xA785, 0xA785), (0xA787, 0xA787), (0xA78C, 0xA78C), (0xA78E, 0xA78E), (0xA791, 0xA791),
    (0xA793, 0xA795), (0xA797, 0xA797), (0xA799, 0xA799), (0xA79B, 0xA79B), (0xA79D, 0xA79D),
    (0xA79F, 0xA79F), (0xA7A1, 0xA7A1), (0xA7A3, 0xA7A3), (0xA7A5, 0xA7A5), (0xA7A7, 0xA7A7),
    (0xA7A9, 0xA7A9), (0xA7AF, 0xA7AF), (0xA7B5, 0xA7B5), (0xA7B7, 0xA7B7), (0xA7B9, 0xA7B9),
    (0xA7BB, 0xA7BB), (0xA7BD, 0xA7BD), (0xA7BF, 0xA7BF), (0xA7C1, 0xA7C1), (0xA7C3, 0xA7C3),
    (0xA7C8, 0xA7C8), (0xA7CA, 0xA7CA), (0xA7CD, 0xA7CD), (0xA7D1, 0xA7D1), (0xA7D3, 0xA7D3),
    (0xA7D5, 0xA7D5), (0xA7D7, 0xA7D7), (0xA7D9, 0xA7D9), (0xA7DB, 0xA7DB), (0xA7F2, 0xA7F4),
    (0xA7F6, 0xA7F6), (0xA7F8, 0xA7FA), (0xAB30, 0xAB5A), (0xAB5C, 0xAB69), (0xAB70, 0xABBF),
    (0xFB00, 0xFB06), (0xFB13, 0xFB17), (0xFF41, 0xFF5A), (0x10428, 0x1044F), (0x104D8, 0x104FB),
    (0x10597, 0x105A1), (0x105A3, 0x105B1), (0x105B3, 0x105B9), (0x105BB, 0x105BC), (0x10780, 0x10780),
    (0x10783, 0x10785), (0x10787, 0x107B0), (0x107B2, 0x107BA), (0x10CC0, 0x10CF2), (0x10D70, 0x10D85),
    (0x118C0, 0x118DF), (0x16E60, 0x16E7F), (0x1D41A, 0x1D433), (0x1D44E, 0x1D454), (0x1D456, 0x1D467),
    (0x1D482, 0x1D49B), (0x1D4B6, 0x1D4B9), (0x1D4BB, 0x1D4BB), (0x1D4BD, 0x1D4C3), (0x1D4C5, 0x1D4CF),
    (0x1D4EA, 0x1D503), (0x1D51E, 0x1D537), (0x1D552, 0x1D56B), (0x1D586, 0x1D59F), (0x1D5BA, 0x1D5D3),
    (0x1D5EE, 0x1D607), (0x1D622, 0x1D63B), (0x1D656, 0x1D66F), (0x1D68A, 0x1D6A5), (0x1D6C2, 0x1D6DA),
    (0x1D6DC, 0x1D6E1), (0x1D6FC, 0x1D714), (0x1D716, 0x1D71B), (0x1D736, 0x1D74E), (0x1D750, 0x1D755),
    (0x1D770, 0x1D788), (0x1D78A, 0x1D78F), (0x1D7AA, 0x1D7C2), (0x1D7C4, 0x1D7C9), (0x1D7CB, 0x1D7CB),
    (0x1DF00, 0x1DF09), (0x1DF0B, 0x1DF1E), (0x1DF25, 0x1DF2A), (0x1E030, 0x1E06D), (0x1E922, 0x1E943),
];

/// A negative `int` reaching these two is NOT the [`native_character_char_count`]
/// hazard. `Character.isUpperCase(-1)` is `false` on HotSpot 25 (measured), a
/// negative widened by `as u32` lands far above `0x10FFFF`, and the binary
/// search therefore misses — the unsigned cast and the JDK agree here. The
/// JDK's `Character` methods do NOT share one out-of-range convention; each
/// one's was read off HotSpot separately.
pub(crate) fn native_character_is_upper_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = in_code_point_runs(JAVA_UPPERCASE_RUNS, ch);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_is_lower_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = in_code_point_runs(JAVA_LOWERCASE_RUNS, ch);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// `Character.toUpperCase` / `toLowerCase` — the SIMPLE case mappings,
/// ENUMERATED from JDK 25 over every plane.
///
/// This replaces a derivation that read three separate facts off Rust's
/// Unicode tables and then corrected them from two override lists:
///
/// * `char::to_uppercase` yields the Unicode **full** mapping
///   (`SpecialCasing.txt`), an iterator; Java wants the **simple** mapping
///   (`UnicodeData.txt` field 12), one code point or none. W7-98(b) recovered
///   most of that with an arity rule (take the mapping only if it is exactly
///   one `char`), which is right for `U+00DF`/`U+FB00..`/`U+0149` and wrong
///   for the 27-member ypogegrammeni family, whose SIMPLE mapping exists
///   (`U+1FB3` -> `U+1FBC`) but which no Rust std API exposes.
/// * `char::to_lowercase` needed the opposite rule — `.next()`, because
///   `U+0130`'s two-char full lowercase begins with its simple one.
/// * Both needed the Latin Extended-D version-skew pins.
///
/// So the old body was a Rust answer plus 33 hand-maintained corrections, and
/// the corrections were derived from a BMP-only sweep. The supplementary
/// planes carry 282 mapped code points in each direction (DESERET, OSAGE,
/// VITHKUQI, LATIN EXTENDED-F/G, OLD HUNGARIAN, GARAY, WARANG CITI,
/// MEDEFAIDRIN, CYRILLIC EXTENDED-D, ADLAM) that no census had ever looked at,
/// and there is no correction list for them because nobody measured one.
///
/// A table generated by executing `Character.toUpperCase(int)` /
/// `toLowerCase(int)` on OpenJDK 25.0.3+9 for all 1,114,112 code points needs
/// no correction list, needs no arity rule, and cannot drift when the
/// toolchain's Unicode version moves. It also makes the two overloads' shared
/// helper total: every input that is not in a run maps to ITSELF, which is
/// what Java specifies for a lone surrogate, for an unassigned code point, and
/// for `0x110000` alike.
///
/// **Encoding.** `(first, last, stride, delta)`: the code points
/// `first, first+stride, first+2*stride, ...` up to `last` each map to
/// themselves plus `delta`; every other code point inside the span, and every
/// code point outside every span, maps to itself. `stride` is what makes the
/// table small — Latin Extended-A is 200-odd alternating case PAIRS
/// (`U+0100`/`U+0101`, `U+0102`/`U+0103`, ...) and one `stride == 2` run
/// covers each block of them. Without it the same content needs 690 and 674
/// plain runs instead of 205 and 187.
///
/// The runs are sorted and disjoint, so [`mapped_in_stride_runs`] binary
/// searches them. 205 runs, 1,477 mapped code points.
#[rustfmt::skip]
const JAVA_TO_UPPER_RUNS: &[(u32, u32, u32, i32)] = &[
    (0x0061, 0x007A, 1, -32), (0x00B5, 0x00B5, 1, 743), (0x00E0, 0x00F6, 1, -32),
    (0x00F8, 0x00FE, 1, -32), (0x00FF, 0x00FF, 1, 121), (0x0101, 0x012F, 2, -1),
    (0x0131, 0x0131, 1, -232), (0x0133, 0x0137, 2, -1), (0x013A, 0x0148, 2, -1),
    (0x014B, 0x0177, 2, -1), (0x017A, 0x017E, 2, -1), (0x017F, 0x017F, 1, -300),
    (0x0180, 0x0180, 1, 195), (0x0183, 0x0185, 2, -1), (0x0188, 0x0188, 1, -1),
    (0x018C, 0x018C, 1, -1), (0x0192, 0x0192, 1, -1), (0x0195, 0x0195, 1, 97),
    (0x0199, 0x0199, 1, -1), (0x019A, 0x019A, 1, 163), (0x019B, 0x019B, 1, 42561),
    (0x019E, 0x019E, 1, 130), (0x01A1, 0x01A5, 2, -1), (0x01A8, 0x01A8, 1, -1),
    (0x01AD, 0x01AD, 1, -1), (0x01B0, 0x01B0, 1, -1), (0x01B4, 0x01B6, 2, -1),
    (0x01B9, 0x01B9, 1, -1), (0x01BD, 0x01BD, 1, -1), (0x01BF, 0x01BF, 1, 56),
    (0x01C5, 0x01C5, 1, -1), (0x01C6, 0x01C6, 1, -2), (0x01C8, 0x01C8, 1, -1),
    (0x01C9, 0x01C9, 1, -2), (0x01CB, 0x01CB, 1, -1), (0x01CC, 0x01CC, 1, -2),
    (0x01CE, 0x01DC, 2, -1), (0x01DD, 0x01DD, 1, -79), (0x01DF, 0x01EF, 2, -1),
    (0x01F2, 0x01F2, 1, -1), (0x01F3, 0x01F3, 1, -2), (0x01F5, 0x01F5, 1, -1),
    (0x01F9, 0x021F, 2, -1), (0x0223, 0x0233, 2, -1), (0x023C, 0x023C, 1, -1),
    (0x023F, 0x0240, 1, 10815), (0x0242, 0x0242, 1, -1), (0x0247, 0x024F, 2, -1),
    (0x0250, 0x0250, 1, 10783), (0x0251, 0x0251, 1, 10780), (0x0252, 0x0252, 1, 10782),
    (0x0253, 0x0253, 1, -210), (0x0254, 0x0254, 1, -206), (0x0256, 0x0257, 1, -205),
    (0x0259, 0x0259, 1, -202), (0x025B, 0x025B, 1, -203), (0x025C, 0x025C, 1, 42319),
    (0x0260, 0x0260, 1, -205), (0x0261, 0x0261, 1, 42315), (0x0263, 0x0263, 1, -207),
    (0x0264, 0x0264, 1, 42343), (0x0265, 0x0265, 1, 42280), (0x0266, 0x0266, 1, 42308),
    (0x0268, 0x0268, 1, -209), (0x0269, 0x0269, 1, -211), (0x026A, 0x026A, 1, 42308),
    (0x026B, 0x026B, 1, 10743), (0x026C, 0x026C, 1, 42305), (0x026F, 0x026F, 1, -211),
    (0x0271, 0x0271, 1, 10749), (0x0272, 0x0272, 1, -213), (0x0275, 0x0275, 1, -214),
    (0x027D, 0x027D, 1, 10727), (0x0280, 0x0280, 1, -218), (0x0282, 0x0282, 1, 42307),
    (0x0283, 0x0283, 1, -218), (0x0287, 0x0287, 1, 42282), (0x0288, 0x0288, 1, -218),
    (0x0289, 0x0289, 1, -69), (0x028A, 0x028B, 1, -217), (0x028C, 0x028C, 1, -71),
    (0x0292, 0x0292, 1, -219), (0x029D, 0x029D, 1, 42261), (0x029E, 0x029E, 1, 42258),
    (0x0345, 0x0345, 1, 84), (0x0371, 0x0373, 2, -1), (0x0377, 0x0377, 1, -1),
    (0x037B, 0x037D, 1, 130), (0x03AC, 0x03AC, 1, -38), (0x03AD, 0x03AF, 1, -37),
    (0x03B1, 0x03C1, 1, -32), (0x03C2, 0x03C2, 1, -31), (0x03C3, 0x03CB, 1, -32),
    (0x03CC, 0x03CC, 1, -64), (0x03CD, 0x03CE, 1, -63), (0x03D0, 0x03D0, 1, -62),
    (0x03D1, 0x03D1, 1, -57), (0x03D5, 0x03D5, 1, -47), (0x03D6, 0x03D6, 1, -54),
    (0x03D7, 0x03D7, 1, -8), (0x03D9, 0x03EF, 2, -1), (0x03F0, 0x03F0, 1, -86),
    (0x03F1, 0x03F1, 1, -80), (0x03F2, 0x03F2, 1, 7), (0x03F3, 0x03F3, 1, -116),
    (0x03F5, 0x03F5, 1, -96), (0x03F8, 0x03F8, 1, -1), (0x03FB, 0x03FB, 1, -1),
    (0x0430, 0x044F, 1, -32), (0x0450, 0x045F, 1, -80), (0x0461, 0x0481, 2, -1),
    (0x048B, 0x04BF, 2, -1), (0x04C2, 0x04CE, 2, -1), (0x04CF, 0x04CF, 1, -15),
    (0x04D1, 0x052F, 2, -1), (0x0561, 0x0586, 1, -48), (0x10D0, 0x10FA, 1, 3008),
    (0x10FD, 0x10FF, 1, 3008), (0x13F8, 0x13FD, 1, -8), (0x1C80, 0x1C80, 1, -6254),
    (0x1C81, 0x1C81, 1, -6253), (0x1C82, 0x1C82, 1, -6244), (0x1C83, 0x1C84, 1, -6242),
    (0x1C85, 0x1C85, 1, -6243), (0x1C86, 0x1C86, 1, -6236), (0x1C87, 0x1C87, 1, -6181),
    (0x1C88, 0x1C88, 1, 35266), (0x1C8A, 0x1C8A, 1, -1), (0x1D79, 0x1D79, 1, 35332),
    (0x1D7D, 0x1D7D, 1, 3814), (0x1D8E, 0x1D8E, 1, 35384), (0x1E01, 0x1E95, 2, -1),
    (0x1E9B, 0x1E9B, 1, -59), (0x1EA1, 0x1EFF, 2, -1), (0x1F00, 0x1F07, 1, 8),
    (0x1F10, 0x1F15, 1, 8), (0x1F20, 0x1F27, 1, 8), (0x1F30, 0x1F37, 1, 8),
    (0x1F40, 0x1F45, 1, 8), (0x1F51, 0x1F57, 2, 8), (0x1F60, 0x1F67, 1, 8),
    (0x1F70, 0x1F71, 1, 74), (0x1F72, 0x1F75, 1, 86), (0x1F76, 0x1F77, 1, 100),
    (0x1F78, 0x1F79, 1, 128), (0x1F7A, 0x1F7B, 1, 112), (0x1F7C, 0x1F7D, 1, 126),
    (0x1F80, 0x1F87, 1, 8), (0x1F90, 0x1F97, 1, 8), (0x1FA0, 0x1FA7, 1, 8),
    (0x1FB0, 0x1FB1, 1, 8), (0x1FB3, 0x1FB3, 1, 9), (0x1FBE, 0x1FBE, 1, -7205),
    (0x1FC3, 0x1FC3, 1, 9), (0x1FD0, 0x1FD1, 1, 8), (0x1FE0, 0x1FE1, 1, 8),
    (0x1FE5, 0x1FE5, 1, 7), (0x1FF3, 0x1FF3, 1, 9), (0x214E, 0x214E, 1, -28),
    (0x2170, 0x217F, 1, -16), (0x2184, 0x2184, 1, -1), (0x24D0, 0x24E9, 1, -26),
    (0x2C30, 0x2C5F, 1, -48), (0x2C61, 0x2C61, 1, -1), (0x2C65, 0x2C65, 1, -10795),
    (0x2C66, 0x2C66, 1, -10792), (0x2C68, 0x2C6C, 2, -1), (0x2C73, 0x2C73, 1, -1),
    (0x2C76, 0x2C76, 1, -1), (0x2C81, 0x2CE3, 2, -1), (0x2CEC, 0x2CEE, 2, -1),
    (0x2CF3, 0x2CF3, 1, -1), (0x2D00, 0x2D25, 1, -7264), (0x2D27, 0x2D27, 1, -7264),
    (0x2D2D, 0x2D2D, 1, -7264), (0xA641, 0xA66D, 2, -1), (0xA681, 0xA69B, 2, -1),
    (0xA723, 0xA72F, 2, -1), (0xA733, 0xA76F, 2, -1), (0xA77A, 0xA77C, 2, -1),
    (0xA77F, 0xA787, 2, -1), (0xA78C, 0xA78C, 1, -1), (0xA791, 0xA793, 2, -1),
    (0xA794, 0xA794, 1, 48), (0xA797, 0xA7A9, 2, -1), (0xA7B5, 0xA7C3, 2, -1),
    (0xA7C8, 0xA7CA, 2, -1), (0xA7CD, 0xA7CD, 1, -1), (0xA7D1, 0xA7D1, 1, -1),
    (0xA7D7, 0xA7DB, 2, -1), (0xA7F6, 0xA7F6, 1, -1), (0xAB53, 0xAB53, 1, -928),
    (0xAB70, 0xABBF, 1, -38864), (0xFF41, 0xFF5A, 1, -32), (0x10428, 0x1044F, 1, -40),
    (0x104D8, 0x104FB, 1, -40), (0x10597, 0x105A1, 1, -39), (0x105A3, 0x105B1, 1, -39),
    (0x105B3, 0x105B9, 1, -39), (0x105BB, 0x105BC, 1, -39), (0x10CC0, 0x10CF2, 1, -64),
    (0x10D70, 0x10D85, 1, -32), (0x118C0, 0x118DF, 1, -32), (0x16E60, 0x16E7F, 1, -32),
    (0x1E922, 0x1E943, 1, -34),
];

/// `Character.toLowerCase`. See [`JAVA_TO_UPPER_RUNS`] for the encoding and
/// the provenance. 187 runs, 1,460 mapped code points.
#[rustfmt::skip]
const JAVA_TO_LOWER_RUNS: &[(u32, u32, u32, i32)] = &[
    (0x0041, 0x005A, 1, 32), (0x00C0, 0x00D6, 1, 32), (0x00D8, 0x00DE, 1, 32),
    (0x0100, 0x012E, 2, 1), (0x0130, 0x0130, 1, -199), (0x0132, 0x0136, 2, 1),
    (0x0139, 0x0147, 2, 1), (0x014A, 0x0176, 2, 1), (0x0178, 0x0178, 1, -121),
    (0x0179, 0x017D, 2, 1), (0x0181, 0x0181, 1, 210), (0x0182, 0x0184, 2, 1),
    (0x0186, 0x0186, 1, 206), (0x0187, 0x0187, 1, 1), (0x0189, 0x018A, 1, 205),
    (0x018B, 0x018B, 1, 1), (0x018E, 0x018E, 1, 79), (0x018F, 0x018F, 1, 202),
    (0x0190, 0x0190, 1, 203), (0x0191, 0x0191, 1, 1), (0x0193, 0x0193, 1, 205),
    (0x0194, 0x0194, 1, 207), (0x0196, 0x0196, 1, 211), (0x0197, 0x0197, 1, 209),
    (0x0198, 0x0198, 1, 1), (0x019C, 0x019C, 1, 211), (0x019D, 0x019D, 1, 213),
    (0x019F, 0x019F, 1, 214), (0x01A0, 0x01A4, 2, 1), (0x01A6, 0x01A6, 1, 218),
    (0x01A7, 0x01A7, 1, 1), (0x01A9, 0x01A9, 1, 218), (0x01AC, 0x01AC, 1, 1),
    (0x01AE, 0x01AE, 1, 218), (0x01AF, 0x01AF, 1, 1), (0x01B1, 0x01B2, 1, 217),
    (0x01B3, 0x01B5, 2, 1), (0x01B7, 0x01B7, 1, 219), (0x01B8, 0x01B8, 1, 1),
    (0x01BC, 0x01BC, 1, 1), (0x01C4, 0x01C4, 1, 2), (0x01C5, 0x01C5, 1, 1),
    (0x01C7, 0x01C7, 1, 2), (0x01C8, 0x01C8, 1, 1), (0x01CA, 0x01CA, 1, 2),
    (0x01CB, 0x01DB, 2, 1), (0x01DE, 0x01EE, 2, 1), (0x01F1, 0x01F1, 1, 2),
    (0x01F2, 0x01F4, 2, 1), (0x01F6, 0x01F6, 1, -97), (0x01F7, 0x01F7, 1, -56),
    (0x01F8, 0x021E, 2, 1), (0x0220, 0x0220, 1, -130), (0x0222, 0x0232, 2, 1),
    (0x023A, 0x023A, 1, 10795), (0x023B, 0x023B, 1, 1), (0x023D, 0x023D, 1, -163),
    (0x023E, 0x023E, 1, 10792), (0x0241, 0x0241, 1, 1), (0x0243, 0x0243, 1, -195),
    (0x0244, 0x0244, 1, 69), (0x0245, 0x0245, 1, 71), (0x0246, 0x024E, 2, 1),
    (0x0370, 0x0372, 2, 1), (0x0376, 0x0376, 1, 1), (0x037F, 0x037F, 1, 116),
    (0x0386, 0x0386, 1, 38), (0x0388, 0x038A, 1, 37), (0x038C, 0x038C, 1, 64),
    (0x038E, 0x038F, 1, 63), (0x0391, 0x03A1, 1, 32), (0x03A3, 0x03AB, 1, 32),
    (0x03CF, 0x03CF, 1, 8), (0x03D8, 0x03EE, 2, 1), (0x03F4, 0x03F4, 1, -60),
    (0x03F7, 0x03F7, 1, 1), (0x03F9, 0x03F9, 1, -7), (0x03FA, 0x03FA, 1, 1),
    (0x03FD, 0x03FF, 1, -130), (0x0400, 0x040F, 1, 80), (0x0410, 0x042F, 1, 32),
    (0x0460, 0x0480, 2, 1), (0x048A, 0x04BE, 2, 1), (0x04C0, 0x04C0, 1, 15),
    (0x04C1, 0x04CD, 2, 1), (0x04D0, 0x052E, 2, 1), (0x0531, 0x0556, 1, 48),
    (0x10A0, 0x10C5, 1, 7264), (0x10C7, 0x10C7, 1, 7264), (0x10CD, 0x10CD, 1, 7264),
    (0x13A0, 0x13EF, 1, 38864), (0x13F0, 0x13F5, 1, 8), (0x1C89, 0x1C89, 1, 1),
    (0x1C90, 0x1CBA, 1, -3008), (0x1CBD, 0x1CBF, 1, -3008), (0x1E00, 0x1E94, 2, 1),
    (0x1E9E, 0x1E9E, 1, -7615), (0x1EA0, 0x1EFE, 2, 1), (0x1F08, 0x1F0F, 1, -8),
    (0x1F18, 0x1F1D, 1, -8), (0x1F28, 0x1F2F, 1, -8), (0x1F38, 0x1F3F, 1, -8),
    (0x1F48, 0x1F4D, 1, -8), (0x1F59, 0x1F5F, 2, -8), (0x1F68, 0x1F6F, 1, -8),
    (0x1F88, 0x1F8F, 1, -8), (0x1F98, 0x1F9F, 1, -8), (0x1FA8, 0x1FAF, 1, -8),
    (0x1FB8, 0x1FB9, 1, -8), (0x1FBA, 0x1FBB, 1, -74), (0x1FBC, 0x1FBC, 1, -9),
    (0x1FC8, 0x1FCB, 1, -86), (0x1FCC, 0x1FCC, 1, -9), (0x1FD8, 0x1FD9, 1, -8),
    (0x1FDA, 0x1FDB, 1, -100), (0x1FE8, 0x1FE9, 1, -8), (0x1FEA, 0x1FEB, 1, -112),
    (0x1FEC, 0x1FEC, 1, -7), (0x1FF8, 0x1FF9, 1, -128), (0x1FFA, 0x1FFB, 1, -126),
    (0x1FFC, 0x1FFC, 1, -9), (0x2126, 0x2126, 1, -7517), (0x212A, 0x212A, 1, -8383),
    (0x212B, 0x212B, 1, -8262), (0x2132, 0x2132, 1, 28), (0x2160, 0x216F, 1, 16),
    (0x2183, 0x2183, 1, 1), (0x24B6, 0x24CF, 1, 26), (0x2C00, 0x2C2F, 1, 48),
    (0x2C60, 0x2C60, 1, 1), (0x2C62, 0x2C62, 1, -10743), (0x2C63, 0x2C63, 1, -3814),
    (0x2C64, 0x2C64, 1, -10727), (0x2C67, 0x2C6B, 2, 1), (0x2C6D, 0x2C6D, 1, -10780),
    (0x2C6E, 0x2C6E, 1, -10749), (0x2C6F, 0x2C6F, 1, -10783), (0x2C70, 0x2C70, 1, -10782),
    (0x2C72, 0x2C72, 1, 1), (0x2C75, 0x2C75, 1, 1), (0x2C7E, 0x2C7F, 1, -10815),
    (0x2C80, 0x2CE2, 2, 1), (0x2CEB, 0x2CED, 2, 1), (0x2CF2, 0x2CF2, 1, 1),
    (0xA640, 0xA66C, 2, 1), (0xA680, 0xA69A, 2, 1), (0xA722, 0xA72E, 2, 1),
    (0xA732, 0xA76E, 2, 1), (0xA779, 0xA77B, 2, 1), (0xA77D, 0xA77D, 1, -35332),
    (0xA77E, 0xA786, 2, 1), (0xA78B, 0xA78B, 1, 1), (0xA78D, 0xA78D, 1, -42280),
    (0xA790, 0xA792, 2, 1), (0xA796, 0xA7A8, 2, 1), (0xA7AA, 0xA7AA, 1, -42308),
    (0xA7AB, 0xA7AB, 1, -42319), (0xA7AC, 0xA7AC, 1, -42315), (0xA7AD, 0xA7AD, 1, -42305),
    (0xA7AE, 0xA7AE, 1, -42308), (0xA7B0, 0xA7B0, 1, -42258), (0xA7B1, 0xA7B1, 1, -42282),
    (0xA7B2, 0xA7B2, 1, -42261), (0xA7B3, 0xA7B3, 1, 928), (0xA7B4, 0xA7C2, 2, 1),
    (0xA7C4, 0xA7C4, 1, -48), (0xA7C5, 0xA7C5, 1, -42307), (0xA7C6, 0xA7C6, 1, -35384),
    (0xA7C7, 0xA7C9, 2, 1), (0xA7CB, 0xA7CB, 1, -42343), (0xA7CC, 0xA7CC, 1, 1),
    (0xA7D0, 0xA7D0, 1, 1), (0xA7D6, 0xA7DA, 2, 1), (0xA7DC, 0xA7DC, 1, -42561),
    (0xA7F5, 0xA7F5, 1, 1), (0xFF21, 0xFF3A, 1, 32), (0x10400, 0x10427, 1, 40),
    (0x104B0, 0x104D3, 1, 40), (0x10570, 0x1057A, 1, 39), (0x1057C, 0x1058A, 1, 39),
    (0x1058C, 0x10592, 1, 39), (0x10594, 0x10595, 1, 39), (0x10C80, 0x10CB2, 1, 64),
    (0x10D50, 0x10D65, 1, 32), (0x118A0, 0x118BF, 1, 32), (0x16E40, 0x16E5F, 1, 32),
    (0x1E900, 0x1E921, 1, 34),
];

/// Apply a `(first, last, stride, delta)` case-mapping table to one code
/// point. Anything the table does not name maps to itself — which is Java's
/// answer for every unmapped code point, including a lone surrogate and
/// including an `int` outside `0..=0x10FFFF`.
#[inline]
fn mapped_in_stride_runs(runs: &[(u32, u32, u32, i32)], cp: u32) -> u32 {
    let found = runs.binary_search_by(|&(lo, hi, _, _)| {
        if hi < cp {
            std::cmp::Ordering::Less
        } else if lo > cp {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    match found {
        Ok(i) => {
            let (lo, _, stride, delta) = runs[i];
            if (cp - lo) % stride == 0 {
                // `checked_` rather than a cast: no reachable input overflows,
                // and an unreachable one must not panic. A Rust panic is not a
                // Java throwable — it takes the VM down from ordinary
                // application bytecode.
                cp.checked_add_signed(delta).unwrap_or(cp)
            } else {
                cp
            }
        }
        Err(_) => cp,
    }
}

/// Shared by `toUpperCase(C)C`/`(I)I` and `toLowerCase(C)C`/`(I)I`, so the
/// overloads cannot drift: the JDK bytecode for `(C)C` is literally
/// `(char) toUpperCase((int) c)`, and a divergence between the two is a defect
/// by construction.
#[inline]
fn character_case_map(ch: u32, upper: bool) -> u32 {
    mapped_in_stride_runs(
        if upper {
            JAVA_TO_UPPER_RUNS
        } else {
            JAVA_TO_LOWER_RUNS
        },
        ch,
    )
}

/// `Character.toUpperCase(char)`.
///
/// W7-98(c) is preserved by construction rather than by a guard: a lone
/// surrogate is a legal `char` and must survive a case mapping unchanged, and
/// `U+D800..U+DFFF` appear in no run of [`JAVA_TO_UPPER_RUNS`], so the lookup
/// returns the input. The body this replaced went through `char::from_u32`,
/// which answers `None` for a surrogate, and finished `.unwrap_or('\0')` —
/// `Character.toUpperCase('\uD800')` answered `U+0000` where HotSpot 25
/// answers `'\uD800'`, i.e. silent data corruption of any UTF-16 pair split on
/// a chunk boundary. There is no longer a `char` in this path to fail on.
pub(crate) fn native_character_to_upper_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(character_case_map(ch, true) as i32)))
}

pub(crate) fn native_character_to_lower_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(character_case_map(ch, false) as i32)))
}

/// `Character.toLowerCase(int)` — code-point variant. Shares
/// [`character_case_map`] with the `(C)C` form so the two cannot drift: the
/// JDK bytecode for `(C)C` is literally `toLowerCase((int) c)` narrowed back to
/// a `char`, so a divergence between the two overloads is a defect by
/// construction. Invalid / out-of-range code points (including a lone
/// surrogate) pass back unchanged.
pub(crate) fn native_character_to_lower_case_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if !(0..=0x10FFFF).contains(&cp) {
        return Ok(Some(Value::Int(cp)));
    }
    Ok(Some(
        Value::Int(character_case_map(cp as u32, false) as i32),
    ))
}

/// `Character.toUpperCase(int)` — code-point variant, mirror of
/// `toLowerCase(I)I`.
///
/// Shares [`character_case_map`] with the `(C)C` form, so both overloads read
/// the same table. History, because the shape of the two old defects is worth
/// keeping: W7-98(b) fixed eleven code points whose Unicode FULL uppercase is
/// multi-char and whose Java answer is therefore "unchanged" (`U+00DF`,
/// `U+FB00..U+FB05`, `U+0149`, `U+01F0`, `U+0390`, `U+03B0`, `U+1E96`,
/// `U+1F50`) with an arity rule, and W7-95(C1) then had to except the 27
/// ypogegrammeni code points the arity rule refuses but Java maps
/// (`U+1FB3` -> `U+1FBC`). E7 measured that arity rule against every
/// supplementary code point: **0** further refusals above the BMP, so the rule
/// was not hiding a second residual up there — but it, both override lists and
/// the Rust lookup underneath them are gone anyway, replaced by
/// [`JAVA_TO_UPPER_RUNS`].
///
/// The `0..=0x10FFFF` guard below is now redundant with the table (an
/// out-of-range `int` is in no run and maps to itself) and is kept because it
/// states the contract at the boundary the JDK states it at: measured on
/// OpenJDK 25.0.3+9, `toUpperCase(0x110000)` and `toUpperCase(Integer.MIN_VALUE)`
/// both return their input.
pub(crate) fn native_character_to_upper_case_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if !(0..=0x10FFFF).contains(&cp) {
        return Ok(Some(Value::Int(cp)));
    }
    Ok(Some(Value::Int(character_case_map(cp as u32, true) as i32)))
}

/// `Character.isLetterOrDigit` — literally `isLetter(c) || isDigit(c)`, which
/// is the JDK's own one-line definition.
///
/// W7-98(a). The old body used `char::is_alphanumeric`, which is
/// `Alphabetic ∪ N*` — so on top of [`native_character_is_letter`]'s `Nl` and
/// `Other_Alphabetic` error it independently added `No`: `U+00B2` SUPERSCRIPT
/// TWO and `U+00BD` VULGAR FRACTION ONE HALF answered `true` where Java answers
/// `false`. Measured over the whole BMP against HotSpot 25, that was **1,257 of
/// 65,536** wrong.
///
/// Composing the two predicates the way the JDK does dropped it to **957** —
/// exactly [`native_character_is_letter`]'s count, i.e. this method contributed
/// NO error of its own. W7-95(C1) then took `isLetter` and `isDigit` to zero, so
/// this one follows to zero with no further change than keeping the composition
/// honest.
pub(crate) fn native_character_is_letter_or_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = in_code_point_runs(JAVA_LETTER_RUNS, ch) || java_is_digit_code_point(ch);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Phase 12: Number wrapper cross-type conversion helpers
// ---------------------------------------------------------------------------

// Int-stored field 0 → Long
pub(crate) fn native_wrapper_int_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v as i64,
        _ => 0,
    };
    Ok(Some(Value::Long(val)))
}

// Int-stored field 0 → Float
pub(crate) fn native_wrapper_int_to_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v as f32,
        _ => 0.0,
    };
    Ok(Some(Value::Float(val)))
}

// Int-stored field 0 → Double
pub(crate) fn native_wrapper_int_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v as f64,
        _ => 0.0,
    };
    Ok(Some(Value::Double(val)))
}

// Long field 0 → Int
pub(crate) fn native_wrapper_long_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Long(v) => v as i32,
        _ => 0,
    };
    Ok(Some(Value::Int(val)))
}

// Long field 0 → Float
pub(crate) fn native_wrapper_long_to_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Long(v) => v as f32,
        _ => 0.0,
    };
    Ok(Some(Value::Float(val)))
}

// Long field 0 → Double
pub(crate) fn native_wrapper_long_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Long(v) => v as f64,
        _ => 0.0,
    };
    Ok(Some(Value::Double(val)))
}

// Float field 0 → Int
pub(crate) fn native_wrapper_float_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Float(v) => v as i32,
        _ => 0,
    };
    Ok(Some(Value::Int(val)))
}

// Float field 0 → Long
pub(crate) fn native_wrapper_float_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Float(v) => v as i64,
        _ => 0,
    };
    Ok(Some(Value::Long(val)))
}

// Float field 0 → Double
pub(crate) fn native_wrapper_float_to_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Float(v) => v as f64,
        _ => 0.0,
    };
    Ok(Some(Value::Double(val)))
}

// Double field 0 → Int
pub(crate) fn native_wrapper_double_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Double(v) => v as i32,
        _ => 0,
    };
    Ok(Some(Value::Int(val)))
}

// Double field 0 → Long
pub(crate) fn native_wrapper_double_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Double(v) => v as i64,
        _ => 0,
    };
    Ok(Some(Value::Long(val)))
}

// Double field 0 → Float
pub(crate) fn native_wrapper_double_to_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Double(v) => v as f32,
        _ => 0.0,
    };
    Ok(Some(Value::Float(val)))
}

// ---------------------------------------------------------------------------
// Phase 12: Wrapper instance toString / hashCode / equals
// ---------------------------------------------------------------------------

// toString for Int-stored wrappers (Integer, Byte, Short)
pub(crate) fn native_wrapper_int_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v,
        _ => 0,
    };
    let s = ctx.create_string(&val.to_string());
    Ok(Some(Value::Object(Some(s))))
}

// toString for Long wrapper
pub(crate) fn native_wrapper_long_to_string_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        _ => 0,
    };
    let s = ctx.create_string(&val.to_string());
    Ok(Some(Value::Object(Some(s))))
}

// toString for Float wrapper
pub(crate) fn native_wrapper_float_to_string_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Float(v) => v,
        _ => 0.0,
    };
    let s = ctx.create_string(&format_float(val));
    Ok(Some(Value::Object(Some(s))))
}

// toString for Double wrapper
pub(crate) fn native_wrapper_double_to_string_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Double(v) => v,
        _ => 0.0,
    };
    let s = ctx.create_string(&format_double(val));
    Ok(Some(Value::Object(Some(s))))
}

// toString for Boolean wrapper
pub(crate) fn native_boolean_instance_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v != 0,
        _ => false,
    };
    let s = ctx.create_string(if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(s))))
}

// toString for Character wrapper
pub(crate) fn native_character_instance_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Delegate to the static twin rather than repeating its body. `char` cannot
    // represent an unpaired surrogate, so the `char::from_u32(val).unwrap_or('\0')`
    // that used to live here answered U+0000 for every one of D800..=DFFF —
    // MEASURED 2026-08-13: `Character.valueOf('\uD800').toString().charAt(0)` was
    // 0 where HotSpot gives 55296, while the STATIC `Character.toString(char)` and
    // `String.valueOf(char)` were both already correct. The static twin has
    // handled this since it was written; this one never called it.
    native_character_static_to_string(ctx, &[Value::Int(val)])
}

// hashCode for Int-stored wrappers (Integer, Byte, Short, Character)
pub(crate) fn native_wrapper_int_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

// hashCode for Boolean wrapper (true=1231, false=1237)
pub(crate) fn native_boolean_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1237))),
    };
    match ctx.get_field(this, 0) {
        Value::Int(v) => Ok(Some(Value::Int(if v != 0 { 1231 } else { 1237 }))),
        _ => Ok(Some(Value::Int(1237))),
    }
}

// hashCode for Long wrapper: (v ^ (v >>> 32)) as i32
pub(crate) fn native_wrapper_long_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int((val ^ ((val as u64 >> 32) as i64)) as i32)))
}

// hashCode for Float wrapper: floatToIntBits(v)
pub(crate) fn native_wrapper_float_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Float(v) => v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(float_to_int_bits_canonical(val) as i32)))
}

// hashCode for Double wrapper: bits = doubleToLongBits; (bits ^ (bits >>> 32)) as i32.
// `doubleToLongBits`, not the raw one: `equals` canonicalizes, so `hashCode`
// must too, or two NaNs that are `equals` land in different hash buckets and a
// `HashMap` keyed on a NaN sentinel silently misses.
pub(crate) fn native_wrapper_double_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = match ctx.get_field(this, 0) {
        Value::Double(v) => v,
        _ => 0.0,
    };
    let bits = double_to_long_bits_canonical(val) as i64;
    Ok(Some(Value::Int(
        (bits ^ ((bits as u64 >> 32) as i64)) as i32,
    )))
}

// Wrapper `equals(Object)` is specified as an **instance test** on the argument
// (`obj instanceof Integer i && value == i.value`), not a bare field-0 payload
// compare. These natives own the whole method — the real JDK bytecode never
// runs — so the type test has to live here. Without it
// `Boolean.FALSE.equals(new int[] {0})` answered `true`: both operands decoded
// to `Int(0)`, the wrapper's `value` and the int array's body read through the
// object-field path. That is what made H2's
// `TestGetGeneratedKeys.testColumnNotFound` skip generated-key validation
// entirely, and reading field 0 of an array argument is also what fired the
// `gen_heap::read_slot: corrupt Value cell` guard — that diagnostic was a
// symptom of this missing check, not of heap corruption.
//
// Every wrapper class is `final`, so `instanceof` is exactly "same class".
fn wrapper_same_class(
    ctx: &mut dyn NativeContext,
    a: cratonvm_types::ObjectRef,
    b: cratonvm_types::ObjectRef,
) -> bool {
    // An ARRAY's header stores its COMPONENT class id, not an id of its own
    // (the same convention the `VirtualNative` cache gate in
    // `interpreter/invoke.rs` guards against for receivers). Comparing raw
    // class ids therefore answers "same class" for `Foo` vs `Foo[]` — so
    // `wrapperInstance.equals(someFooArray)` passed this gate and the callers
    // below read slot 0 of the ARRAY through the legacy 16-byte field path,
    // decoding two adjacent 8-byte element references as a single `Value`.
    // That is what produced the `gen_heap::read_slot: corrupt Value cell
    // (out-of-range discriminant)` reports in
    // docs/known-issues/h2/bug-h2-testgetgeneratedkeys-corrupt-value-cell-hib-cv-32-family.md:
    // the heap was intact (a valid `String[2]`), the READER used the wrong
    // accessor. An array is never a boxed primitive wrapper, so decline.
    if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array
        || ctx.heap_kind_of(b) == cratonvm_types::ObjectKind::Array
    {
        return false;
    }
    let ca = ctx.class_id_of_object(a);
    let cb = ctx.class_id_of_object(b);
    if ca == cb {
        return true;
    }
    // Cold path: distinct ids can still name the same class when more than one
    // copy is loaded (isolating loaders), so compare names before answering no.
    match (ctx.class_name_of_id(ca), ctx.class_name_of_id(cb)) {
        (Some(na), Some(nb)) => na == nb,
        _ => false,
    }
}

// equals for Int-stored wrappers (Integer, Boolean, Character, Byte, Short)
pub(crate) fn native_wrapper_int_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !wrapper_same_class(ctx, this, other) {
        return Ok(Some(Value::Int(0)));
    }
    // Require the stored value to actually BE an int, exactly as
    // `native_wrapper_long_equals` below already does for `Value::Long`. A
    // raw `Value` comparison treats "both reads decoded as something else"
    // (a non-wrapper receiver, or the benign `Object(None)` the `gen_heap`
    // guards substitute for an out-of-bounds/undecodable slot) as EQUAL.
    let a = match ctx.get_field(this, 0) {
        Value::Int(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match ctx.get_field(other, 0) {
        Value::Int(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

// equals for Long wrapper
pub(crate) fn native_wrapper_long_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !wrapper_same_class(ctx, this, other) {
        return Ok(Some(Value::Int(0)));
    }
    let a = match ctx.get_field(this, 0) {
        Value::Long(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match ctx.get_field(other, 0) {
        Value::Long(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

// equals for Float wrapper (NaN == NaN is true per Float.equals spec, using to_bits)
pub(crate) fn native_wrapper_float_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !wrapper_same_class(ctx, this, other) {
        return Ok(Some(Value::Int(0)));
    }
    let a = match ctx.get_field(this, 0) {
        Value::Float(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match ctx.get_field(other, 0) {
        Value::Float(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        if float_to_int_bits_canonical(a) == float_to_int_bits_canonical(b) {
            1
        } else {
            0
        },
    )))
}

// `Double.equals` is `doubleToLongBits(value) == doubleToLongBits(other.value)`.
// It is the CANONICALIZING conversion, so `NaN.equals(NaN)` is true for any two
// NaNs — not only for two copies of the same bit pattern, which is all that raw
// `to_bits` gave. See `double_to_long_bits_canonical`.
pub(crate) fn native_wrapper_double_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if !wrapper_same_class(ctx, this, other) {
        return Ok(Some(Value::Int(0)));
    }
    let a = match ctx.get_field(this, 0) {
        Value::Double(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match ctx.get_field(other, 0) {
        Value::Double(v) => v,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(
        if double_to_long_bits_canonical(a) == double_to_long_bits_canonical(b) {
            1
        } else {
            0
        },
    )))
}

// ---------------------------------------------------------------------------
// Phase 12: Character additional methods
// ---------------------------------------------------------------------------

/// `Character.digit(char|int, int)` — out-of-range radix answers -1, it does
/// not throw and must not abort.
///
/// Measured against real JDK 25: `digit('7', 0)`, `digit('7', 1)`,
/// `digit('7', -1)`, `digit('7', 37)`, `digit('7', 40)`,
/// `digit('7', Integer.MIN_VALUE)` and `digit('7', Integer.MAX_VALUE)` all
/// return -1. This is a THIRD radix contract, distinct from both
/// `toString`'s substitute-10 and `parseInt`'s NumberFormatException.
///
/// The radix used to be read as `*v as u32` and handed straight to
/// `char::to_digit`, which PANICS for a radix above 36 — so `Character.digit`
/// with a negative radix (which became a huge `u32`) or any radix > 36 aborted
/// the VM from ordinary Java code.
/// `Character.digit(char, int)`.
///
/// W7-98(a). `char::to_digit` is ASCII-only, so this answered `-1` for every
/// non-ASCII decimal digit: **360 of 65,536** BMP code points disagreed with
/// HotSpot 25. Route it through [`java_char_digit`] — the JDK-25-generated run
/// table that was already in this file and had only one caller — which
/// reproduces `Character.digit(char, 10)` over the entire BMP with **zero**
/// mismatches.
pub(crate) fn native_character_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 10,
    };
    if !(2..=36).contains(&radix) {
        return Ok(Some(Value::Int(-1)));
    }
    let result = char::from_u32(ch)
        .and_then(|c| java_char_digit(c, radix as u32))
        .map(|d| d as i32)
        .unwrap_or(-1);
    Ok(Some(Value::Int(result)))
}

/// `Character.forDigit(int, int)` — out-of-range digit or radix answers the
/// NUL character, it does not throw and must not abort.
///
/// Measured against real JDK 25: `forDigit(d, r)` is `'\0'` for every `r`
/// outside 2..=36 (including 0, 1, -1, 37, 40, `Integer.MIN_VALUE`,
/// `Integer.MAX_VALUE`) and for every `d` outside `0..r`; `forDigit(0, 2)` is
/// `'0'` and `forDigit(35, 36)` is `'z'`.
///
/// Same defect as [`native_character_digit`]: `char::from_digit` PANICS for a
/// radix above 36, and the radix reached it through an unchecked `as u32`.
pub(crate) fn native_character_for_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let digit = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(0))),
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 10,
    };
    if !(2..=36).contains(&radix) {
        return Ok(Some(Value::Int(0)));
    }
    let result = char::from_digit(digit, radix as u32).unwrap_or('\0') as i32;
    Ok(Some(Value::Int(result)))
}

/// `Character.getNumericValue(char)`.
///
/// W7-98(a). PARTIAL FIX, and the residual is stated rather than hidden.
///
/// The old body used `char::to_digit(36)`, which is ASCII-only: **784 of
/// 65,536** BMP code points disagreed with HotSpot 25. Routing through
/// [`java_char_digit`] picks up every non-ASCII `Nd` run and takes that to
/// **372**.
///
/// W7-95(C1) closes the 372. They were the part `JAVA_DIGIT_RUNS` does not
/// model, because `Character.digit` does not either — and reusing the digit
/// table for a *numeric value* was the category error:
///
/// * `Nl`/`No` numeric values — `U+2160` ROMAN NUMERAL ONE is `1`, `U+00B2`
///   SUPERSCRIPT TWO is `2`; `Character.digit` says `-1` for both, correctly,
///   because they are not digits in any radix.
/// * the `-2` sentinel Java returns for a code point whose numeric value is not
///   a non-negative integer (`U+00BD` VULGAR FRACTION ONE HALF), which the
///   digit table has no way to express at all.
///
/// [`JAVA_NUMERIC_VALUE_RUNS`] + [`JAVA_NUMERIC_VALUE_NEG2_RUNS`] are the JDK's
/// own answer, generated by walking the BMP on JDK 25. BMP is the whole domain:
/// only `(C)I` is registered, so no argument can exceed `U+FFFF`.
pub(crate) fn native_character_get_numeric_value(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let result = match value_in_runs(JAVA_NUMERIC_VALUE_RUNS, ch) {
        Some(v) => v,
        None if in_code_point_runs(JAVA_NUMERIC_VALUE_NEG2_RUNS, ch) => -2,
        None => -1,
    };
    Ok(Some(Value::Int(result)))
}

/// `Character.charCount(int)` — the ONE member of this family whose compare is
/// **signed**.
///
/// The javadoc is one line and has no error case: `codePoint >=
/// MIN_SUPPLEMENTARY_CODE_POINT ? 2 : 1`. There is no range validation, no
/// throw, and no "invalid code point" answer — `charCount(-1)` is `1` and
/// `charCount(Integer.MAX_VALUE)` is `2`. Measured on OpenJDK 25.0.3+9 over
/// `MIN_VALUE, -2147483647, -65536, -2, -1, 0, 0xFFFF, 0x10000, 0x10FFFF,
/// 0x110000, 0x7FFFFFFF`: every negative answers `1`, everything from
/// `0x10000` up answers `2`.
///
/// The old body read the argument as `*v as u32` and compared `cp > 0xFFFF`.
/// That cast is where the sign went: `-1` widens to `0xFFFF_FFFF`, which is
/// above `MIN_SUPPLEMENTARY`, so **every** negative `int` answered `2` where
/// HotSpot answers `1`. It is a wrong answer, not a panic — this method cannot
/// panic and never could.
///
/// **This is not a family-wide rule, and it must not be applied as one.** The
/// neighbours' unsigned casts are their JDK contracts, not copies of this bug:
/// [`native_character_is_bmp_code_point`] is `(codePoint >>> 16) == 0` in the
/// JDK — an UNSIGNED shift, correctly `false` for a negative — and
/// `isValidCodePoint` is likewise unsigned in the JDK
/// (`(plane << 16) < (MAX_CODE_POINT + 1)` over `codePoint >>> 16`). Rewriting
/// those two "the same way" would turn two correct members into regressions.
/// Each contract in this family was read off HotSpot separately; the transcript
/// is in `docs/known-issues/jdk-only/E7-1-character-int-code-point-contracts.md`.
pub(crate) fn native_character_char_count(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if cp >= 0x10000 { 2 } else { 1 })))
}

pub(crate) fn native_character_is_high_surrogate(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if (0xD800..=0xDBFF).contains(&ch) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_character_is_low_surrogate(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if (0xDC00..=0xDFFF).contains(&ch) {
        1
    } else {
        0
    })))
}

/// `Character.isBmpCodePoint(int)`. **The `as u32` here is CORRECT and is the
/// JDK's own arithmetic — do not "fix" it to match
/// [`native_character_char_count`].**
///
/// The JDK body is `(codePoint >>> 16) == 0`: an unsigned shift, so every
/// negative `int` has a nonzero high half and answers `false`. Widening to
/// `u32` and testing `cp <= 0xFFFF` is the same predicate over the same 2^32
/// inputs. Measured on OpenJDK 25.0.3+9: `false` for `MIN_VALUE`, `-65536`,
/// `-1`, `0x110000` and `0x7FFFFFFF`; `true` for `0..=0xFFFF` including every
/// lone surrogate.
///
/// `Character.toChars`/`toString(int)` are NOT registered here — they run JDK
/// bytecode — and both reach their `IllegalArgumentException` through this
/// predicate and `isValidCodePoint`. A signed rewrite of this body would make
/// `Character.toChars(-1)` return `new char[]{(char) 0xFFFF}` instead of throwing.
pub(crate) fn native_character_is_bmp_code_point(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if cp <= 0xFFFF { 1 } else { 0 })))
}

/// `Character.isValidCodePoint(int)`. The JDK writes this unsigned too
/// (`(codePoint >>> 16) < ((MAX_CODE_POINT + 1) >>> 16)`); the signed
/// `0..=0x10FFFF` below is the same predicate over all 2^32 inputs, because a
/// negative's unsigned high half is at least `0x8000`. Measured on OpenJDK
/// 25.0.3+9: `false` for `MIN_VALUE`, `-1`, `0x110000`, `0x7FFFFFFF`; `true`
/// for `0`, `0xD800`, `0xFFFF`, `0x10000`, `0x10FFFF`.
pub(crate) fn native_character_is_valid_code_point(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => -1,
    };
    Ok(Some(Value::Int(if (0..=0x10FFFF).contains(&cp) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_character_is_iso_control(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => -1,
    };
    Ok(Some(Value::Int(
        if (0x00..=0x1F).contains(&cp) || (0x7F..=0x9F).contains(&cp) {
            1
        } else {
            0
        },
    )))
}

/// `Character.toString(char)`.
///
/// W7-98(c). A lone surrogate is a legal `char` but is NOT a Unicode scalar
/// value, so it cannot round-trip through Rust's `char`/`str`: the old body's
/// `char::from_u32(ch).unwrap_or('\0')` turned `U+D800` into `U+0000`, and even
/// without that `unwrap_or`, `ctx.create_string(&str)` has no way to express
/// one. Measured against HotSpot 25 on `U+D800/U+DBFF/U+DC00/U+DFFF`:
/// `Character.toString(c).charAt(0)` answered `0` here and the input there.
///
/// `String.valueOf(char)` is NOT native-registered (verified by
/// `--dump-native-registry`), so this is a plain call into real JDK bytecode
/// with no native re-entry — and it is measured correct on this VM for every
/// lone surrogate. The Rust path stays as a fallback for the synthetic class
/// library, where that bytecode does not exist; it is still exact for every
/// scalar value, which is every input except the 2,048 surrogates.
pub(crate) fn native_character_static_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    match char::from_u32(ch) {
        Some(c) => {
            let s = ctx.create_string(&c.to_string());
            Ok(Some(Value::Object(Some(s))))
        }
        None => {
            // Surrogate: hand it to the real `String.valueOf(char)`, which
            // stores UTF-16 code units and preserves it.
            if let Ok(Some(v @ Value::Object(Some(_)))) = ctx.invoke(
                "java/lang/String",
                "valueOf",
                "(C)Ljava/lang/String;",
                &[Value::Int(ch as i32)],
            ) {
                return Ok(Some(v));
            }
            // No real class library: keep the historical answer rather than
            // failing the call.
            let s = ctx.create_string("\u{0}");
            Ok(Some(Value::Object(Some(s))))
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 12: Boolean additional methods
// ---------------------------------------------------------------------------

pub(crate) fn native_boolean_parse_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    Ok(Some(Value::Int(if text.eq_ignore_ascii_case("true") {
        1
    } else {
        0
    })))
}

pub(crate) fn native_boolean_static_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let s = ctx.create_string(if val { "true" } else { "false" });
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn native_boolean_static_hash_code(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    Ok(Some(Value::Int(if val { 1231 } else { 1237 })))
}

pub(crate) fn native_boolean_compare(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    Ok(Some(Value::Int(match (a, b) {
        (true, false) => 1,
        (false, true) => -1,
        _ => 0,
    })))
}

/// `Boolean.getBoolean(String name)` — JDK semantics:
///   returns `parseBoolean(System.getProperty(name))`, swallowing any
///   IllegalArgumentException / NullPointerException to `false`.
///
/// Effectively: true iff the system property exists and equals "true"
/// (case-insensitive).  A null/absent property yields false.
pub(crate) fn native_boolean_get_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // null or missing argument → false (matches JDK's NPE-swallow).
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = ctx.read_string(name_obj).unwrap_or_default();
    let matches = ctx
        .get_system_property(&key)
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    Ok(Some(Value::Int(if matches { 1 } else { 0 })))
}

// --- Integer/Long radix helpers ---

/// `Integer.toString(int, int)` — THIS is the body that runs.
///
/// Registration chain (verified 2026-08-07, and the reason two earlier fixes
/// missed): `register` is last-write-wins per `(class, method, descriptor)`
/// triple (`native-api/src/registry.rs`, the `Some(prior_slot)` arm assigns
/// `slot.callback = callback`). `register_essential_natives_with_shims`
/// registers `java/lang/Integer.toString(II)` inline (`lib.rs` ~:11114) and
/// then calls `lang_math::register_wrapper_natives` (`lib.rs` ~:14260), which
/// registers it AGAIN at `lang_math.rs` ~:598 — so this function wins in
/// essential/real-JDK mode. `register_synthetic_overrides` (`lib.rs` :21085)
/// calls `register_wrapper_natives` again at ~:22915, so it wins in synthetic
/// mode too. Nothing else in the tree registers this triple.
///
/// The body delegates to `crate::java_int_to_string_radix` rather than
/// formatting here. There used to be a separate local digit loop
/// (`i64_to_radix_string`); it took the radix as an already-cast `u32`, so
/// `Integer.toString(5, 40)` panicked the VM inside `char::from_digit`,
/// `Integer.toString(5, 1)` spun forever building an unbounded digit buffer,
/// and `Integer.toString(5, 0)` divided by zero. One shared body per width is
/// the point: a second one is what let those three survive two rounds of
/// fixes in the shadowed copies.
pub(crate) fn native_integer_to_string_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 10,
    };
    let text = crate::java_int_to_string_radix(val, radix);
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_long_parse_long_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let text = read_string_arg_nfe(ctx, args)?;
    let radix = parse_radix_arg(args)?;
    let v = java_parse_into(&text, radix, i64::MIN, i64::MAX, false)?;
    Ok(Some(Value::Long(v)))
}

/// `Long.toString(long, int)` — THIS is the body that runs; same
/// last-write-wins chain as [`native_integer_to_string_radix`], registered at
/// `lang_math.rs` ~:612 after the `lib.rs` ~:11097 copy.
///
/// Delegates to `crate::java_long_to_string_radix`, which handles `i64::MIN`
/// by widening to `i128` before taking the magnitude (the deleted local body
/// relied on `wrapping_neg` plus an `as u64` reinterpretation) and, crucially,
/// normalizes the radix through `crate::java_radix_or_ten` first.
pub(crate) fn native_long_to_string_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 10,
    };
    let text = crate::java_long_to_string_radix(val, radix);
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

// --- Float ---

pub(crate) fn native_float_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Float");
    ctx.set_field(obj, 0, Value::Float(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_float_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let val = ctx.get_field(this, 0);
    match val {
        Value::Float(_) => Ok(Some(val)),
        _ => Ok(Some(Value::Float(0.0))),
    }
}

pub(crate) fn native_float_int_bits_to_float(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let bits = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Float(f32::from_bits(bits as u32))))
}

pub(crate) fn native_float_is_nan(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_nan() { 1 } else { 0 })))
}

pub(crate) fn native_float_is_infinite(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_infinite() { 1 } else { 0 })))
}

// --- Double (boxing) ---

pub(crate) fn native_double_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Double");
    ctx.set_field(obj, 0, Value::Double(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_double_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let val = ctx.get_field(this, 0);
    match val {
        Value::Double(_) => Ok(Some(val)),
        _ => Ok(Some(Value::Double(0.0))),
    }
}

pub(crate) fn native_double_is_nan(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_nan() { 1 } else { 0 })))
}

pub(crate) fn native_double_is_infinite(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_infinite() { 1 } else { 0 })))
}

// --- Float/Double parsing and utilities (Phase 8 Part 7) ---

// ---------------------------------------------------------------------------
// Java's floating-point grammar
//
// `str::parse::<f64>` is NOT `Double.parseDouble`, and again it differs in
// both directions:
//
//   * Rust accepts `nan` / `inf` / `infinity` CASE-INSENSITIVELY. Java accepts
//     only the exact spellings `NaN` and `Infinity`; `Double.parseDouble("inf")`
//     throws on a real JDK. We were answering +Infinity — a wrong VALUE, not
//     just a missing exception, for any input-validation path.
//   * Rust REJECTS Java's hex significand, `0x1p3` == 8.0.
//   * Rust's `str::trim` strips Unicode whitespace; the Java grammar's
//     `[\x00-\x20]*` does not. `Double.parseDouble("\u{a0}1.0")` throws on a
//     real JDK; we answered 1.0.
//
// The grammar implemented here is the regex published in the
// `Double.valueOf(String)` javadoc, transcribed rather than approximated:
//
// ```text
// [\x00-\x20]* [+-]? ( NaN | Infinity |
//     ( ( Digits (\.)? Digits? Exp? )
//     | ( \. Digits Exp? )
//     | ( ( 0[xX] HexDigits (\.)? | 0[xX] HexDigits? \. HexDigits ) [pP] [+-]? Digits )
//     ) [fFdD]? ) [\x00-\x20]*
// ```
//
// `Digits` is `\p{Digit}`, which WITHOUT `UNICODE_CHARACTER_CLASS` is ASCII
// `[0-9]` only — so unlike `Integer.parseInt`, the floating-point grammar does
// NOT accept Unicode decimal digits. Measured on JDK 25:
// `Double.parseDouble("\u{661}\u{662}")` throws while
// `Integer.parseInt("\u{661}\u{662}")` returns 12. The two grammars really do
// disagree, and copying one onto the other is how this drifted.
// ---------------------------------------------------------------------------

/// `String.trim()` semantics: strip chars `<= '\u{20}'`, which is exactly the
/// grammar's `[\x00-\x20]*`. Deliberately NOT `str::trim`, which also strips
/// NBSP and the rest of Unicode `White_Space`.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= '\u{20}')
}

/// The shared front half of `Double.parseDouble` / `Float.parseFloat`: trim,
/// sign, the two literal words, and the optional `FloatTypeSuffix`.
enum JavaFloatHead<'a> {
    /// One of the two words. `nan` is true for `NaN`, else `Infinity`.
    Word {
        nan: bool,
        neg: bool,
    },
    /// A numeric body with its sign, suffix already removed.
    Body {
        body: &'a str,
        neg: bool,
    },
    Malformed,
}

fn java_float_head(s: &str) -> JavaFloatHead<'_> {
    let t = java_trim(s);
    if t.is_empty() {
        return JavaFloatHead::Malformed;
    }
    // `t` is non-empty and the sign is ASCII, so slicing at 1 is on a char
    // boundary.
    let (neg, rest) = match t.as_bytes()[0] {
        b'+' => (false, &t[1..]),
        b'-' => (true, &t[1..]),
        _ => (false, t),
    };
    if rest == "NaN" {
        return JavaFloatHead::Word { nan: true, neg };
    }
    if rest == "Infinity" {
        return JavaFloatHead::Word { nan: false, neg };
    }
    if rest.is_empty() {
        return JavaFloatHead::Malformed;
    }
    let body = match rest.as_bytes()[rest.len() - 1] {
        b'f' | b'F' | b'd' | b'D' => &rest[..rest.len() - 1],
        _ => rest,
    };
    if body.is_empty() {
        return JavaFloatHead::Malformed;
    }
    JavaFloatHead::Body { body, neg }
}

/// `Digits (\.)? Digits? Exp?` | `\. Digits Exp?` — the decimal alternatives.
///
/// Only a validator: everything it accepts is also accepted by Rust's
/// `f64`/`f32` `from_str`, whose grammar is a strict superset over the decimal
/// forms and which is correctly rounded, so the actual conversion is delegated.
/// The point of the check is to reject what Rust would otherwise ACCEPT.
fn java_decimal_grammar_ok(b: &str) -> bool {
    let s = b.as_bytes();
    let n = s.len();
    let mut i = 0;
    let mut int_digits = 0;
    while i < n && s[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0;
    if i < n && s[i] == b'.' {
        i += 1;
        while i < n && s[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < n {
        if s[i] != b'e' && s[i] != b'E' {
            return false;
        }
        i += 1;
        if i < n && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0;
        while i < n && s[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return false;
        }
    }
    i == n
}

/// `HexDigits (\.)?` | `HexDigits? \. HexDigits`, then a MANDATORY
/// `[pP] [+-]? Digits`. `b` is the body with the leading `0x`/`0X` removed.
///
/// Returns the significand hex digits, how many of them follow the point, and
/// the binary exponent.
fn java_hex_grammar(b: &str) -> Option<(Vec<u8>, usize, i64)> {
    let s = b.as_bytes();
    let n = s.len();
    let mut i = 0;
    let mut digits: Vec<u8> = Vec::new();
    while i < n {
        let Some(d) = (s[i] as char).to_digit(16) else {
            break;
        };
        digits.push(d as u8);
        i += 1;
    }
    let int_n = digits.len();
    let mut frac_n = 0usize;
    if i < n && s[i] == b'.' {
        i += 1;
        while i < n {
            let Some(d) = (s[i] as char).to_digit(16) else {
                break;
            };
            digits.push(d as u8);
            frac_n += 1;
            i += 1;
        }
    }
    if int_n == 0 && frac_n == 0 {
        return None;
    }
    // The binary exponent is not optional in this alternative.
    if i >= n || (s[i] != b'p' && s[i] != b'P') {
        return None;
    }
    i += 1;
    let mut exp_neg = false;
    if i < n && (s[i] == b'+' || s[i] == b'-') {
        exp_neg = s[i] == b'-';
        i += 1;
    }
    let mut exp_digits = 0;
    let mut pexp: i64 = 0;
    while i < n && s[i].is_ascii_digit() {
        // Saturate rather than overflow: any exponent past this is far beyond
        // the range where the result is not already 0 or Infinity.
        if pexp < 1_000_000 {
            pexp = pexp * 10 + (s[i] - b'0') as i64;
        }
        i += 1;
        exp_digits += 1;
    }
    if exp_digits == 0 || i != n {
        return None;
    }
    Some((digits, frac_n, if exp_neg { -pexp } else { pexp }))
}

/// Round `m * 2^exp2` — with `sticky` recording that nonzero bits were already
/// dropped off the bottom of `m` — to the nearest IEEE-754 binary value of the
/// given width, ties to even, and return the raw bit pattern of its MAGNITUDE
/// (the caller ORs in the sign).
///
/// `prec` is the significand width in bits (53 for `double`, 24 for `float`)
/// and `emax` the maximum normal exponent (1023 / 127), from which the bias
/// and the minimum normal exponent `1 - emax` follow.
///
/// SHIFT INVARIANT — read before touching the early returns. This is the one
/// place in this file where a Java-supplied string drives a shift COUNT
/// (`Double.parseDouble("0x…p…")` reaches here with a caller-chosen binary
/// exponent), and Rust panics — aborting the VM — on a shift at or above the
/// integer width. Three guards interlock to keep every shift in range:
///
///   * `e > emax + 1` and `e < qmin - 2` bound `e`, hence bound `shift` to
///     roughly `nb`;
///   * `shift > nb` returns early, so `shift <= nb <= 128`;
///   * the `sh >= 128` arms below handle the single surviving `shift == 128`
///     case, where `m >> 128` would panic.
///
/// Together these also keep `-shift` under `prec` on the left-shift arm.
/// Loosening any one of them can reintroduce a shift-overflow abort reachable
/// from ordinary bytecode.
fn round_binary(m: u128, sticky: bool, exp2: i64, prec: u32, emax: i64) -> u64 {
    let inf_bits = ((2 * emax + 1) as u64) << (prec - 1);
    if m == 0 {
        return 0;
    }
    let nb = (128 - m.leading_zeros()) as i64; // bit length of m
    let e = exp2 + nb - 1; // value == 1.f * 2^e, exactly
    let emin = 1 - emax; // minimum NORMAL exponent
    let qmin = emin - (prec as i64 - 1); // subnormal quantum (-1074 / -149)

    // These bounds also keep `shift` within the integer width. `e == qmin - 1`
    // and `e == qmin - 2` must stay in the general path: the first can still
    // round up to MIN_VALUE and the second is where the exact tie lands.
    if e > emax + 1 {
        return inf_bits;
    }
    if e < qmin - 2 {
        return 0;
    }

    // Quantum of the result significand: in the normal range it tracks `e`; in
    // the subnormal range it is pinned at `qmin`.
    let q = if e >= emin {
        e - (prec as i64 - 1)
    } else {
        qmin
    };
    let shift = q - exp2; // bits of m to drop
    if shift > nb {
        return 0; // strictly below half
    }

    let (mut s, round_up) = if shift > 0 {
        let sh = shift as u32;
        let trunc = if sh >= 128 { 0 } else { m >> sh };
        let low = if sh >= 128 {
            m
        } else {
            m & ((1u128 << sh) - 1)
        };
        let half = 1u128 << (sh - 1);
        let up = match low.cmp(&half) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            // Exactly half: any bit already shifted out breaks the tie
            // upward, otherwise round to even.
            std::cmp::Ordering::Equal => sticky || (trunc & 1 == 1),
        };
        (trunc, up)
    } else {
        (m << ((-shift) as u32), false)
    };
    if round_up {
        s += 1;
    }
    if s == 0 {
        return 0;
    }

    if q == qmin {
        // Subnormal encoding. A carry that took `s` up to exactly 2^(prec-1)
        // IS the MIN_NORMAL bit pattern — the subnormal/normal boundary is
        // seamless in IEEE-754, so there is nothing to renormalize.
        return s as u64;
    }
    let mut e = e;
    if 128 - s.leading_zeros() > prec {
        s >>= 1;
        e += 1;
    }
    if e > emax {
        return inf_bits;
    }
    (((e + emax) as u64) << (prec - 1)) | ((s as u64) & ((1u64 << (prec - 1)) - 1))
}

/// Convert a parsed hex significand to raw bits at the requested width.
///
/// `value == M * 2^(pexp - 4*frac_n)` where `M` is the significand digits read
/// as one integer. `M` is accumulated into a `u128`; once it is full the
/// remaining digits only contribute to the binary exponent and to a sticky
/// bit, which is all the rounding needs.
fn java_hex_float_bits(digits: &[u8], frac_n: usize, pexp: i64, prec: u32, emax: i64) -> u64 {
    let mut m: u128 = 0;
    let mut extra: i64 = 0;
    let mut sticky = false;
    let mut started = false;
    for &d in digits {
        if !started && d == 0 {
            continue; // leading zeros carry no information
        }
        started = true;
        if m.leading_zeros() >= 4 {
            m = (m << 4) | d as u128;
        } else {
            extra += 4;
            sticky |= d != 0;
        }
    }
    if !started {
        return 0; // a significand of all zeros is zero at any exponent
    }
    let exp2 = pexp - 4 * frac_n as i64 + extra;
    round_binary(m, sticky, exp2, prec, emax)
}

fn java_nfe_float(s: &str) -> cratonvm_types::error::RuntimeError {
    cratonvm_types::error::RuntimeError::NumberFormatException {
        message: format!("For input string: \"{s}\""),
    }
}

/// Read argument 0 as a non-null `String` for the FLOATING-point parse family.
///
/// `Double.parseDouble(null)` and `Float.parseFloat(null)` throw
/// `NullPointerException`, not `NumberFormatException` — they reach
/// `String.length()`/`charAt` on the null before any grammar check. The
/// INTEGER family is the other way round and throws
/// `NumberFormatException("Cannot parse null string")`; see
/// `read_string_arg_nfe`. Measured on JDK 25, both ways.
fn read_string_arg_npe(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    match args.first() {
        Some(Value::Object(Some(obj))) => Ok(ctx.read_string(*obj).unwrap_or_default()),
        // HotSpot's helpful NPE, naming the JDK's own local in
        // `FloatingDecimal.readJavaFormatString`. Shared by all four entry
        // points — `Double`/`Float` x `parseX`/`valueOf` — which is measured,
        // not assumed: this helper serves all four and they all say `"in"`.
        _ => Err(cratonvm_types::error::RuntimeError::NullPointerException {
            message: Some("Cannot invoke \"String.length()\" because \"in\" is null".to_string()),
        }
        .into()),
    }
}

fn parse_float_string(s: &str) -> Result<f32, cratonvm_types::error::RuntimeError> {
    if s.trim().is_empty() {
        return Err(java_nfe_empty());
    }
    let (body, neg) = match java_float_head(s) {
        JavaFloatHead::Malformed => return Err(java_nfe_float(s)),
        JavaFloatHead::Word { nan: true, .. } => return Ok(f32::NAN),
        JavaFloatHead::Word { nan: false, neg } => {
            return Ok(if neg {
                f32::NEG_INFINITY
            } else {
                f32::INFINITY
            })
        }
        JavaFloatHead::Body { body, neg } => (body, neg),
    };
    let is_hex = body.len() > 1
        && body.as_bytes()[0] == b'0'
        && (body.as_bytes()[1] == b'x' || body.as_bytes()[1] == b'X');
    let v = if is_hex {
        let (digits, frac_n, pexp) =
            java_hex_grammar(&body[2..]).ok_or_else(|| java_nfe_float(s))?;
        f32::from_bits(java_hex_float_bits(&digits, frac_n, pexp, 24, 127) as u32)
    } else {
        if !java_decimal_grammar_ok(body) {
            return Err(java_nfe_float(s));
        }
        // Parsed at float width directly, NOT via `f64` — narrowing a double
        // would round twice and can land on the wrong float.
        body.parse::<f32>().map_err(|_| java_nfe_float(s))?
    };
    Ok(if neg { -v } else { v })
}

/// `NumberFormatException("empty String")` — the message the floating-point
/// parsers use for an input that is empty AFTER trimming, in place of the
/// `For input string: "..."` every other malformed input gets.
///
/// Measured across all four entry points (`Double`/`Float` x
/// `parseX`/`valueOf`) and for a BLANK string as well as an empty one: the JDK
/// trims first and then finds nothing, so `"   "` is also "empty String". The
/// integral parsers do NOT share this — `Integer.parseInt("")` is
/// `For input string: ""` — which is why this cannot be hoisted.
fn java_nfe_empty() -> cratonvm_types::error::RuntimeError {
    cratonvm_types::error::RuntimeError::NumberFormatException {
        message: "empty String".to_string(),
    }
}

fn parse_double_string(s: &str) -> Result<f64, cratonvm_types::error::RuntimeError> {
    if s.trim().is_empty() {
        return Err(java_nfe_empty());
    }
    let (body, neg) = match java_float_head(s) {
        JavaFloatHead::Malformed => return Err(java_nfe_float(s)),
        JavaFloatHead::Word { nan: true, .. } => return Ok(f64::NAN),
        JavaFloatHead::Word { nan: false, neg } => {
            return Ok(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            })
        }
        JavaFloatHead::Body { body, neg } => (body, neg),
    };
    let is_hex = body.len() > 1
        && body.as_bytes()[0] == b'0'
        && (body.as_bytes()[1] == b'x' || body.as_bytes()[1] == b'X');
    let v = if is_hex {
        let (digits, frac_n, pexp) =
            java_hex_grammar(&body[2..]).ok_or_else(|| java_nfe_float(s))?;
        f64::from_bits(java_hex_float_bits(&digits, frac_n, pexp, 53, 1023))
    } else {
        if !java_decimal_grammar_ok(body) {
            return Err(java_nfe_float(s));
        }
        body.parse::<f64>().map_err(|_| java_nfe_float(s))?
    };
    Ok(if neg { -v } else { v })
}
pub(crate) fn native_float_parse_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = read_string_arg_npe(ctx, args)?;
    let val = parse_float_string(&s)?;
    Ok(Some(Value::Float(val)))
}

pub(crate) fn native_double_parse_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = read_string_arg_npe(ctx, args)?;
    let val = parse_double_string(&s)?;
    Ok(Some(Value::Double(val)))
}

pub(crate) fn native_float_value_of_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = read_string_arg_npe(ctx, args)?;
    let val = parse_float_string(&s)?;
    let obj = alloc_wrapper(ctx, "java/lang/Float");
    ctx.set_field(obj, 0, Value::Float(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_double_value_of_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = read_string_arg_npe(ctx, args)?;
    let val = parse_double_string(&s)?;
    let obj = alloc_wrapper(ctx, "java/lang/Double");
    ctx.set_field(obj, 0, Value::Double(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_float_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let s = format_float(v);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_double_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let s = format_double(v);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_float_to_int_bits(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let f = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // Canonical NaN: all NaN values map to 0x7fc00000
    let bits = if f.is_nan() {
        0x7fc00000_i32
    } else {
        f.to_bits() as i32
    };
    Ok(Some(Value::Int(bits)))
}

pub(crate) fn native_double_to_long_bits(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let d = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Canonical NaN: all NaN values map to 0x7ff8000000000000
    let bits = if d.is_nan() {
        0x7ff8000000000000_i64
    } else {
        d.to_bits() as i64
    };
    Ok(Some(Value::Long(bits)))
}

pub(crate) fn native_float_compare(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(java_compare_float(a, b))))
}

pub(crate) fn native_double_compare(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(java_compare_double(a, b))))
}

// --- Byte ---

/// `Byte.valueOf(byte)` — the ONE member of the family with no uncached arm.
///
/// `jdk25src/java.base/java/lang/Byte.java` is
/// `return ByteCache.cache[(int)b + 128];` with no range test, because the
/// cache's 256 slots already cover every `byte`. Measured on HotSpot 25.0.3+9:
/// `Byte.valueOf(b) == Byte.valueOf(b)` for all 256 values including
/// `Byte.MIN_VALUE`. The previous body allocated every time, so all 256 were
/// wrong here.
///
/// The range guard below is not a semantic bound (there is none) — it is an
/// index guard. The descriptor is `(B)`, so a well-formed call always lands in
/// the cache; a malformed one falls back to the old fresh-allocation behaviour
/// instead of indexing off the end of the array.
pub(crate) fn native_byte_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if (-128..=127).contains(&val) {
        let obj = cached_wrapper_box(
            ctx,
            byte_cache(),
            (val + 128) as usize,
            "java/lang/Byte",
            Value::Int(val),
        );
        return Ok(Some(Value::Object(Some(obj))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Byte");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

// --- Short ---

/// `Short.valueOf(short)` — cached over -128..=127 only, out of 65,536 values.
///
/// `jdk25src/java.base/java/lang/Short.java`:
/// `if (sAsInt >= -128 && sAsInt <= 127) return ShortCache.cache[sAsInt + 128];`
/// Same numeric bound as `Integer`/`Long`, a DIFFERENT bound from `Byte`
/// (which has no bound) and from `Character` (which has no negative half).
/// Measured on HotSpot 25.0.3+9 by walking `Short.MIN_VALUE..=Short.MAX_VALUE`:
/// the identical range came back as exactly -128..127, and `valueOf((short)128)`
/// / `valueOf((short)-129)` are fresh objects.
pub(crate) fn native_short_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if (-128..=127).contains(&val) {
        let obj = cached_wrapper_box(
            ctx,
            short_cache(),
            (val + 128) as usize,
            "java/lang/Short",
            Value::Int(val),
        );
        return Ok(Some(Value::Object(Some(obj))));
    }
    let obj = alloc_wrapper(ctx, "java/lang/Short");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

// ===========================================================================
// Integer.compare / Integer.compareTo / Long.compare / Long.compareTo
// ===========================================================================

pub(crate) fn native_integer_compare(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(a.cmp(&b) as i32)))
}

pub(crate) fn native_integer_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this_val = match args.first() {
        Some(Value::Object(Some(r))) => match ctx.get_field(*r, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
        _ => 0,
    };
    let other_val = match args.get(1) {
        Some(Value::Object(Some(r))) => match ctx.get_field(*r, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Value::Int(this_val.cmp(&other_val) as i32)))
}

pub(crate) fn native_long_compare(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(a.cmp(&b) as i32)))
}

pub(crate) fn native_long_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this_val = match args.first() {
        Some(Value::Object(Some(r))) => match ctx.get_field(*r, 0) {
            Value::Long(v) => v,
            _ => 0,
        },
        _ => 0,
    };
    let other_val = match args.get(1) {
        Some(Value::Object(Some(r))) => match ctx.get_field(*r, 0) {
            Value::Long(v) => v,
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Value::Int(this_val.cmp(&other_val) as i32)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

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
                if y == 0.0 || y == 1.0 || y == 2.0 || y == -1.0 {
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
                // anything.) W7-54-strictmath-fdlibm-family.md.
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
}

/// Radix conformance for the natives that ACTUALLY dispatch.
///
/// `lib.rs`'s `radix_to_string_tests` covers the shared helpers
/// (`java_int_to_string_radix` / `java_long_to_string_radix`). Those tests
/// could not have caught this defect and did not: the helpers were already
/// correct, while the bodies that ran were the `native_*` entry points in
/// THIS file, registered last into a last-write-wins registry and therefore
/// shadowing the `lib.rs` copies. Two separate fix attempts landed on the
/// shadowed copies. Everything below drives the live entry points through the
/// same `(args) -> Value` shape the interpreter uses.
///
/// Every expectation was read off real JDK 25.0.3 (`java RadixProbe.java`),
/// not derived from this implementation.
#[cfg(test)]
mod radix_native_entrypoint_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    // `read_string` / `create_string` live on NativeHeapAccess; it is not in
    // this module's prelude, so the trait must be imported for method
    // resolution on the concrete MockNativeContext.
    use cratonvm_native_api::NativeHeapAccess;

    /// Drive a String-returning native and read the answer back out of the
    /// mock heap.
    fn as_text(
        f: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult,
        args: &[Value],
    ) -> String {
        let mut ctx = mock_ctx();
        let out = f(&mut ctx, args).expect("this native never throws");
        match out {
            Some(Value::Object(Some(o))) => ctx.read_string(o).expect("a readable String"),
            other => panic!("expected a String result, got {other:?}"),
        }
    }

    fn int_to_string(val: i32, radix: i32) -> String {
        as_text(
            native_integer_to_string_radix,
            &[Value::Int(val), Value::Int(radix)],
        )
    }

    fn long_to_string(val: i64, radix: i32) -> String {
        as_text(
            native_long_to_string_radix,
            &[Value::Long(val), Value::Int(radix)],
        )
    }

    /// The `toString` family IGNORES an out-of-range radix and uses 10. It
    /// does not throw, and it does not clamp into 2..=36 — a clamp would
    /// answer "11111111" for `toString(255, 0)` where the JDK says "255".
    #[test]
    fn out_of_range_radix_substitutes_ten_and_never_clamps() {
        for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
            assert_eq!(int_to_string(5, bad), "5", "radix {bad}");
            assert_eq!(int_to_string(-5, bad), "-5", "radix {bad}");
            assert_eq!(int_to_string(0, bad), "0", "radix {bad}");
            assert_eq!(int_to_string(-1, bad), "-1", "radix {bad}");
            assert_eq!(int_to_string(255, bad), "255", "radix {bad}");
            assert_eq!(int_to_string(i32::MIN, bad), "-2147483648", "radix {bad}");
            assert_eq!(int_to_string(i32::MAX, bad), "2147483647", "radix {bad}");
            assert_eq!(long_to_string(5, bad), "5", "radix {bad}");
            assert_eq!(long_to_string(-5, bad), "-5", "radix {bad}");
            assert_eq!(long_to_string(0, bad), "0", "radix {bad}");
            assert_eq!(long_to_string(-1, bad), "-1", "radix {bad}");
            assert_eq!(long_to_string(255, bad), "255", "radix {bad}");
            assert_eq!(
                long_to_string(i64::MIN, bad),
                "-9223372036854775808",
                "radix {bad}"
            );
            assert_eq!(
                long_to_string(i64::MAX, bad),
                "9223372036854775807",
                "radix {bad}"
            );
        }
        // A clamp would have produced these instead. Named so a future
        // `clamp(2, 36)` cannot pass by looking plausible.
        assert_ne!(int_to_string(255, 0), "11111111");
        assert_ne!(int_to_string(255, -1), "73");
    }

    /// BOUNDED ON PURPOSE. Radix 0 divides by zero and radix 1 never
    /// terminates in an unguarded digit loop (it also grows the digit buffer
    /// without limit), and radix > 36 aborts inside `char::from_digit`. Run on
    /// a worker against a deadline so a regression FAILS this test — a hang
    /// becomes a timeout and an abort becomes a channel disconnect — instead
    /// of wedging or killing the whole suite.
    ///
    /// The mock context is built INSIDE the worker: `MockNativeContext` holds
    /// `UnsafeCell`s and a raw pointer, so it is not `Send`. Only `String`
    /// crosses the channel.
    #[test]
    fn hostile_radices_terminate_within_a_deadline() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let out = vec![
                int_to_string(5, 0),
                int_to_string(5, 1),
                int_to_string(-5, 1),
                int_to_string(255, 0),
                int_to_string(i32::MIN, 0),
                int_to_string(i32::MIN, 1),
                int_to_string(5, 40),
                int_to_string(5, -1),
                int_to_string(5, i32::MIN),
                long_to_string(5, 0),
                long_to_string(5, 1),
                long_to_string(i64::MIN, 1),
                long_to_string(i64::MIN, 40),
                long_to_string(-1, -1),
            ];
            let _ = tx.send(out);
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("radix 0/1/negative/>36 must terminate and must not abort");
        assert_eq!(
            out,
            vec![
                "5",
                "5",
                "-5",
                "255",
                "-2147483648",
                "-2147483648",
                "5",
                "5",
                "5",
                "5",
                "5",
                "-9223372036854775808",
                "-9223372036854775808",
                "-1",
            ]
        );
        worker.join().expect("worker thread panicked");
    }

    /// Both boundary radices, and the digits either side of them.
    #[test]
    fn boundary_radices_two_and_thirty_six() {
        assert_eq!(int_to_string(255, 2), "11111111");
        assert_eq!(int_to_string(255, 36), "73");
        assert_eq!(int_to_string(5, 2), "101");
        assert_eq!(int_to_string(35, 36), "z");
        assert_eq!(long_to_string(255, 2), "11111111");
        assert_eq!(long_to_string(255, 36), "73");
        assert_eq!(long_to_string(35, 36), "z");
        // 37 is NOT a legal radix, so it falls back to 10 rather than
        // extending the alphabet.
        assert_eq!(int_to_string(36, 37), "36");
    }

    /// Negatives are sign-magnitude, never a two's-complement bit pattern.
    #[test]
    fn negatives_are_sign_magnitude() {
        assert_eq!(int_to_string(-1, 16), "-1");
        assert_eq!(int_to_string(-1, 2), "-1");
        assert_eq!(int_to_string(-255, 16), "-ff");
        assert_eq!(int_to_string(-5, 2), "-101");
        assert_eq!(long_to_string(-1, 16), "-1");
        assert_eq!(long_to_string(-255, 16), "-ff");
        assert_ne!(int_to_string(-1, 16), "ffffffff");
        assert_ne!(long_to_string(-1, 16), "ffffffffffffffff");
    }

    /// `MIN_VALUE` has no positive counterpart, so the magnitude has to be
    /// taken at a wider width. Checked at EVERY legal radix, plus the two
    /// literals the JDK prints at radix 36.
    #[test]
    fn min_value_at_every_legal_radix() {
        assert_eq!(int_to_string(i32::MIN, 36), "-zik0zk");
        assert_eq!(long_to_string(i64::MIN, 36), "-1y2p0ij32e8e8");
        assert_eq!(int_to_string(i32::MIN, 16), "-80000000");
        assert_eq!(long_to_string(i64::MIN, 16), "-8000000000000000");
        for r in 2..=36i32 {
            let s = int_to_string(i32::MIN, r);
            let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
            assert_eq!(
                u32::from_str_radix(mag, r as u32),
                Ok(2_147_483_648u32),
                "int radix {r}"
            );
            let s = long_to_string(i64::MIN, r);
            let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
            assert_eq!(
                u64::from_str_radix(mag, r as u32),
                Ok(9_223_372_036_854_775_808u64),
                "long radix {r}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Character.digit / Character.forDigit — a THIRD radix contract
    // -----------------------------------------------------------------------

    fn digit(ch: char, radix: i32) -> i32 {
        let mut ctx = mock_ctx();
        let out = native_character_digit(&mut ctx, &[Value::Int(ch as i32), Value::Int(radix)])
            .expect("Character.digit never throws");
        match out {
            Some(Value::Int(v)) => v,
            other => panic!("expected Int, got {other:?}"),
        }
    }

    fn for_digit(d: i32, radix: i32) -> i32 {
        let mut ctx = mock_ctx();
        let out = native_character_for_digit(&mut ctx, &[Value::Int(d), Value::Int(radix)])
            .expect("Character.forDigit never throws");
        match out {
            Some(Value::Int(v)) => v,
            other => panic!("expected Int, got {other:?}"),
        }
    }

    /// Out of range answers -1 / NUL — it neither throws nor aborts. Bounded
    /// for the same reason as above: the old bodies handed the radix straight
    /// to `char::to_digit` / `char::from_digit`, which PANIC above 36, and a
    /// negative radix arrived there as a huge `u32`.
    #[test]
    fn character_digit_and_for_digit_reject_hostile_radices_within_a_deadline() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut out = Vec::new();
            for r in [-1, 0, 1, 37, 40, i32::MIN, i32::MAX] {
                out.push(digit('7', r));
                out.push(digit('z', r));
                out.push(for_digit(0, r));
                out.push(for_digit(5, r));
            }
            let _ = tx.send(out);
        });
        let out = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("Character.digit/forDigit must not abort on a bad radix");
        assert!(
            out.iter().enumerate().all(|(i, &v)| {
                // digit -> -1, forDigit -> 0 (the NUL char)
                if i % 4 < 2 {
                    v == -1
                } else {
                    v == 0
                }
            }),
            "every out-of-range radix must answer -1 / NUL, got {out:?}"
        );
        worker.join().expect("worker thread panicked");
    }

    /// The in-range behaviour the guard must not have broken.
    #[test]
    fn character_digit_and_for_digit_in_range() {
        assert_eq!(digit('7', 10), 7);
        assert_eq!(digit('7', 16), 7);
        assert_eq!(digit('7', 36), 7);
        assert_eq!(digit('z', 36), 35);
        // 'z' is not a digit below base 36, and '7' is not one in base 2.
        assert_eq!(digit('z', 16), -1);
        assert_eq!(digit('z', 10), -1);
        assert_eq!(digit('7', 2), -1);
        assert_eq!(digit('?', 36), -1);

        assert_eq!(for_digit(0, 2), '0' as i32);
        assert_eq!(for_digit(0, 10), '0' as i32);
        assert_eq!(for_digit(5, 10), '5' as i32);
        assert_eq!(for_digit(35, 36), 'z' as i32);
        // A digit outside 0..radix is NUL even when the radix is legal.
        assert_eq!(for_digit(5, 2), 0);
        assert_eq!(for_digit(35, 16), 0);
        assert_eq!(for_digit(36, 36), 0);
        assert_eq!(for_digit(-1, 36), 0);
    }

    // -----------------------------------------------------------------------
    // parse*(String, int) — the radix contract INVERTS here
    // -----------------------------------------------------------------------

    /// `parseInt` and friends THROW `NumberFormatException` on an
    /// out-of-range radix; they do NOT substitute 10. Measured message text
    /// included, because the two rules are one `if` apart and the message is
    /// the only thing that distinguishes "bad radix" from "bad digits".
    #[test]
    fn parse_radix_throws_rather_than_substituting_ten() {
        assert_eq!(java_parse_radix_or_nfe(2), Ok(2));
        assert_eq!(java_parse_radix_or_nfe(10), Ok(10));
        assert_eq!(java_parse_radix_or_nfe(36), Ok(36));
        for low in [1, 0, -1, i32::MIN] {
            assert_eq!(
                java_parse_radix_or_nfe(low),
                Err(format!("radix {low} less than Character.MIN_RADIX"))
            );
        }
        for high in [37, 40, i32::MAX] {
            assert_eq!(
                java_parse_radix_or_nfe(high),
                Err(format!("radix {high} greater than Character.MAX_RADIX"))
            );
        }
    }

    /// Bounded: `<int>::from_str_radix` PANICS outside 2..=36, and the radix
    /// used to reach it via an unchecked `as u32`, so `Integer.parseInt("5",
    /// 0)` from ordinary Java bytecode aborted the VM. Each call must now come
    /// back as an ordinary `Err`.
    #[test]
    fn parse_natives_reject_hostile_radices_within_a_deadline() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut ctx = mock_ctx();
            let s = ctx.create_string("5");
            let mut threw = Vec::new();
            for r in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
                let args = [Value::Object(Some(s)), Value::Int(r)];
                threw.push(native_integer_parse_int_radix(&mut ctx, &args).is_err());
                threw.push(native_long_parse_long_radix(&mut ctx, &args).is_err());
                threw.push(native_byte_parse_byte_radix(&mut ctx, &args).is_err());
                threw.push(native_short_parse_short_radix(&mut ctx, &args).is_err());
            }
            // …and the legal radices still parse.
            let ok = [2i32, 10, 36].iter().all(|&r| {
                let args = [Value::Object(Some(s)), Value::Int(r)];
                // "5" is not a base-2 digit string, so only 10 and 36 parse.
                native_integer_parse_int_radix(&mut ctx, &args).is_ok() == (r != 2)
            });
            let _ = tx.send((threw, ok));
        });
        let (threw, ok) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("parse* with a bad radix must return Err, not abort");
        assert!(
            threw.iter().all(|&t| t),
            "every out-of-range radix must throw NumberFormatException, got {threw:?}"
        );
        assert!(
            ok,
            "legal radices must still parse (and base 2 must reject \"5\")"
        );
        worker.join().expect("worker thread panicked");
    }
}
