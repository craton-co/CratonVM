// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Math, StrictMath, and Number subclass native method implementations.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use crate::lang_string::{
    format_double, format_float, native_string_chars, native_string_code_point_at,
    native_string_code_point_count, native_string_format, native_string_format_locale,
    native_string_formatted, native_string_indent, native_string_is_blank, native_string_lines,
    native_string_offset_by_code_points, native_string_region_matches,
    native_string_region_matches_ic, native_string_repeat, native_string_transform,
    native_string_value_of_int, native_string_value_of_long, native_string_value_of_object,
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
    // `Math` keeps libm on purpose. Do NOT "simplify" this by pointing both
    // classes at the fdlibm bodies: that would be slower for the overwhelmingly
    // more common caller and would not fix anything, since libm already
    // satisfies `Math`'s contract.
    let strict = class == "java/lang/StrictMath";
    if strict {
        registry.register(class, "pow", "(DD)D", native_strict_math_pow);
        registry.register(class, "sin", "(D)D", native_strict_math_sin);
        registry.register(class, "cos", "(D)D", native_strict_math_cos);
        registry.register(class, "tan", "(D)D", native_strict_math_tan);
        registry.register(class, "asin", "(D)D", native_strict_math_asin);
        registry.register(class, "acos", "(D)D", native_strict_math_acos);
        registry.register(class, "atan", "(D)D", native_strict_math_atan);
        registry.register(class, "atan2", "(DD)D", native_strict_math_atan2);
        registry.register(class, "log", "(D)D", native_strict_math_log);
        registry.register(class, "log10", "(D)D", native_strict_math_log10);
        registry.register(class, "exp", "(D)D", native_strict_math_exp);
    } else {
        registry.register(class, "pow", "(DD)D", native_math_pow);
        registry.register(class, "sin", "(D)D", native_math_sin);
        registry.register(class, "cos", "(D)D", native_math_cos);
        registry.register(class, "tan", "(D)D", native_math_tan);
        registry.register(class, "asin", "(D)D", native_math_asin);
        registry.register(class, "acos", "(D)D", native_math_acos);
        registry.register(class, "atan", "(D)D", native_math_atan);
        registry.register(class, "atan2", "(DD)D", native_math_atan2);
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
        registry.register(class, "cbrt", "(D)D", native_strict_math_cbrt);
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
    // Same Math/StrictMath split as the block above, and for the same reason:
    // `sinh` and `cosh` are the second- and third-worst rows in the census
    // (28.08% and 28.55% of sampled inputs disagreed with fdlibm on the libm we
    // link), because both are defined in terms of `expm1`/`exp` and inherit
    // those functions' deviation on top of their own.
    if strict {
        registry.register(class, "hypot", "(DD)D", native_strict_math_hypot);
        registry.register(class, "log1p", "(D)D", native_strict_math_log1p);
        registry.register(class, "expm1", "(D)D", native_strict_math_expm1);
        registry.register(class, "sinh", "(D)D", native_strict_math_sinh);
        registry.register(class, "cosh", "(D)D", native_strict_math_cosh);
        registry.register(class, "tanh", "(D)D", native_strict_math_tanh);
    } else {
        registry.register(class, "hypot", "(DD)D", native_math_hypot);
        registry.register(class, "log1p", "(D)D", native_math_log1p);
        registry.register(class, "expm1", "(D)D", native_math_expm1);
        registry.register(class, "sinh", "(D)D", native_math_sinh);
        registry.register(class, "cosh", "(D)D", native_math_cosh);
        registry.register(class, "tanh", "(D)D", native_math_tanh);
    }
    registry.register(class, "copySign", "(DD)D", native_math_copy_sign_double);
    registry.register(class, "copySign", "(FF)F", native_math_copy_sign_float);
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
    Ok(Some(Value::Float(java_math_max_f32(a, b))))
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
    Ok(Some(Value::Double(java_math_max_f64(a, b))))
}

// --- Java min/max semantics for the floating widths ---
//
// Rust's `f64::min`/`f32::min` are IEEE `minNum`: they RETURN THE NON-NaN
// OPERAND, and `<`/`==` cannot see the sign of a zero. Java specifies the
// opposite on both counts — NaN propagates, and `-0.0` sorts strictly below
// `+0.0`.
//
// Measured 2026-08-12 against HotSpot 25.0.3+9, same host, same class file:
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
//
// Why nothing caught it: the enclosing registrar opens with
// `set_category(NativeKind::Intrinsic)`, and `Intrinsic` is exempt from shadow
// retirement AND is not the census's `native-shadows-bytecode` kind — so a
// `--jdk-only-report` run of a program calling `Math.min` four times yields
// ZERO `java/lang/Math` rows. The correct tree already existed in-tree as
// `phases_late::streams::p56_java_math_min`, whose doc comment describes this
// exact trap, with one caller: the positive half fixed, the twin left.

/// `java.lang.Math.min(double,double)` — NOT Rust's `f64::min`.
#[inline(always)]
pub(crate) fn java_math_min_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a <= b { a } else { b }
}

/// `java.lang.Math.max(double,double)` — the mirror of [`java_math_min_f64`].
#[inline(always)]
pub(crate) fn java_math_max_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a >= b { a } else { b }
}

/// `java.lang.Math.min(float,float)` — same contract, `f32` width.
#[inline(always)]
pub(crate) fn java_math_min_f32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { a } else { b };
    }
    if a <= b { a } else { b }
}

/// `java.lang.Math.max(float,float)` — the mirror of [`java_math_min_f32`].
#[inline(always)]
pub(crate) fn java_math_max_f32(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        return f32::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() { b } else { a };
    }
    if a >= b { a } else { b }
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
    Ok(Some(Value::Float(java_math_min_f32(a, b))))
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
    Ok(Some(Value::Double(java_math_min_f64(a, b))))
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
                "Separate from the `Math` body on purpose: `Math`'s contract is an accuracy ",
                "bound (1 ULP, semi-monotonic) that libm meets, while `StrictMath`'s is \"the ",
                "fdlibm result, on every platform and every VM\". Sharing one body made the ",
                "strict class no stricter than the loose one."
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
                "platform libm. See the note on the unary family."
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

strict_math_unary! {
    native_strict_math_sin => sin,
    native_strict_math_cos => cos,
    native_strict_math_tan => tan,
    native_strict_math_asin => asin,
    native_strict_math_acos => acos,
    native_strict_math_atan => atan,
    native_strict_math_exp => exp,
    native_strict_math_log => log,
    native_strict_math_log10 => log10,
    native_strict_math_cbrt => cbrt,
    native_strict_math_log1p => log1p,
    native_strict_math_expm1 => expm1,
    native_strict_math_sinh => sinh,
    native_strict_math_cosh => cosh,
    native_strict_math_tanh => tanh,
}

// Argument order here is load-bearing and is NOT the alphabetical one: the JDK
// declares `atan2(double y, double x)` — ordinate first — and `fdlibm::atan2`
// takes them in that same order, so `args[0]` is `y`. Transposing them is a
// defect no accuracy test can see, because `atan2(y, x)` and `atan2(x, y)` are
// both plausible angles; only the quadrant is wrong. The golden vectors in
// `types/src/fdlibm.rs` cross the full sign matrix, which is what catches it.
strict_math_binary! {
    native_strict_math_atan2 => atan2,
    native_strict_math_pow => pow,
    native_strict_math_hypot => hypot,
}

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
    // JLS special values that C99's pow() — which is what Rust's `powf`
    // lowers to — answers DIFFERENTLY. Falling through to `powf` does not
    // "preserve JLS semantics" for these; it is precisely where the two
    // standards disagree, and it must be pre-empted:
    //
    //   * "If the second argument is NaN, then the result is NaN." C99
    //     instead makes pow(1.0, anything) == 1.0, including pow(1.0, NaN).
    //   * "If the absolute value of the first argument equals 1 and the
    //     second argument is infinite, then the result is NaN." C99 makes
    //     pow(±1.0, ±inf) == 1.0.
    //
    // Measured against HotSpot 25: all five of pow(1.0, NaN),
    // pow(±1.0, ±Infinity) answered 0x3ff0000000000000 (1.0) here and
    // 0x7ff8000000000000 (NaN) there.
    //
    // The neighbouring JLS rule "if the second argument is ±0 the result is
    // 1.0, even for a NaN first argument" is shared by BOTH standards and is
    // deliberately NOT caught here — b == ±0.0 is neither NaN nor infinite,
    // so it falls through untouched.
    if b.is_nan() || (b.is_infinite() && a.abs() == 1.0) {
        return Ok(Some(Value::Double(f64::NAN)));
    }
    // W7-95(C1). THE FAST PATH USED TO BE `a.powi(b as i32)` FOR EVERY
    // INTEGRAL `|b| < 64`, AND THAT BREAKS `Math.pow`'S ACCURACY CONTRACT.
    //
    // `Math.pow` promises "within 1 ulp of the exact result". `f64::powi` is
    // binary exponentiation — up to eleven chained multiplications for `|b|`
    // near 63 — and each one rounds. Measured against the EXACT power
    // (`BigDecimal.pow` at 120 digits), 20,000 random bases per exponent:
    //
    //     |b|      worst error, ulp      cases over the 1-ulp bound
    //       2            0.500                      0 / 20000
    //      -2            1.439                    412 / 20000
    //       3            1.228                    150 / 20000
    //      -3            2.242                   1304 / 20000
    //       4            1.852                   2787 / 20000
    //       8            4.943                  10107 / 20000
    //      16           10.420                  14939 / 20000
    //      32           20.814                  17360 / 20000
    //      63           44.321                  18682 / 20000
    //
    // HotSpot 25's `Math.pow` is within 0.503 ulp on every one of those same
    // inputs, so each of these is also a straight differential divergence. The
    // old comment called this "HotSpot-style"; HotSpot's C2 specialises
    // `pow(x, 2)` and `pow(x, 0.5)`, not a 63-wide window.
    //
    // What survives is the one case that is provably exact: `a * a` is a
    // SINGLE correctly-rounded multiply, hence the correctly-rounded square,
    // hence 0.5 ulp — measured 0.500 worst, 0 violations. It needs no guard on
    // `a`, because it is also right on the specials: `±inf * ±inf == +inf` and
    // `-0.0 * -0.0 == +0.0` are exactly the JLS's answers for `pow(±inf, 2.0)`
    // and `pow(-0.0, 2.0)`, and `NaN * NaN` is NaN.
    //
    // Everything else goes to `powf`, i.e. to the host libm — which is what
    // the doc block at the head of this file says `Math` should use, on the
    // grounds that libm already satisfies `Math`'s 1-ulp contract. `powi` was
    // the one place that bypassed libm and the one place that did not.
    if b == 2.0 {
        return Ok(Some(Value::Double(a * a)));
    }
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
pub(crate) fn native_math_asin(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.asin())))
}

#[inline]
pub(crate) fn native_math_acos(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.acos())))
}

#[inline]
pub(crate) fn native_math_atan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.atan())))
}

#[inline]
pub(crate) fn native_math_atan2(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(a.atan2(b))))
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
    // Java Math.rint: round to nearest even (banker's rounding)
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
    let result = if v.is_nan() {
        f64::NAN
    } else if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        v
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
    let result = if v.is_nan() {
        f32::NAN
    } else if v > 0.0 {
        1.0
    } else if v < 0.0 {
        -1.0
    } else {
        v
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
    // Java floorDiv: rounds toward negative infinity.
    //
    // WRAPPING, not `/` and `%`. `Integer.MIN_VALUE / -1` overflows `i32`, and
    // **Rust checks division overflow unconditionally — in release as well as
    // debug**. The resulting panic is not a Java throwable, so it does not
    // become an `ArithmeticException`: it terminates the VM. One line of
    // ordinary application bytecode, in the default mode, with no flags.
    //
    // JVMS §6.5 (`idiv`/`irem`) specifies the wrap: `MIN_VALUE / -1` is
    // `MIN_VALUE` and `MIN_VALUE % -1` is `0`. HotSpot 25, measured, agrees.
    //
    // The `b == 0` guard directly above is the tell — it handles the divisor
    // *Java* forbids and not the one *Rust* forbids. Same shape in all four
    // bodies here (`floorDiv`/`floorMod` × `int`/`long`), and `StrictMath`
    // registers onto these same bodies, so it was eight fatal triples.
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
    // Wrapping, for the reason spelled out on the `int` body above:
    // `Long.MIN_VALUE / -1` overflows and Rust panics on it in release.
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
    // Java floorMod: a - floorDiv(a,b) * b. Wrapping, for the reason on the
    // `floorDiv` int body above — `MIN_VALUE % -1` overflows and Rust panics on
    // it in release, killing the VM rather than raising anything catchable.
    // JVMS §6.5: the answer is 0.
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) {
        r.wrapping_add(b)
    } else {
        r
    };
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
    // Wrapping — see the `floorDiv` int body above.
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) {
        r.wrapping_add(b)
    } else {
        r
    };
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
pub(crate) fn native_math_hypot(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(a.hypot(b))))
}

#[inline]
pub(crate) fn native_math_log1p(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ln_1p())))
}

#[inline]
pub(crate) fn native_math_expm1(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.exp_m1())))
}

#[inline]
pub(crate) fn native_math_sinh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sinh())))
}

#[inline]
pub(crate) fn native_math_cosh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cosh())))
}

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
        f64::NAN
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
        f64::NAN
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
        f64::NAN
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
    // W7-95(C1). The JDK's own `Math.ulp` answers `Math.abs(d)` for the whole
    // `MAX_EXPONENT + 1` case — NaN and both infinities — which for a NaN
    // PRESERVES ITS PAYLOAD and only clears the sign. Measured on HotSpot 25:
    // `Math.ulp(0x7ff0000000000001)` is `0x7ff0000000000001`, not the canonical
    // `0x7ff8000000000000` that `f64::NAN` would have produced, and
    // `Math.ulp(0xfff8000000000000)` is `0x7ff8000000000000`. Only "is NaN" is
    // specified, so this is fidelity rather than a contract — but `v.abs()` is
    // both the JDK's expression and strictly closer to it, so there is no
    // reason to write anything else.
    let result = if v.is_nan() || v.is_infinite() {
        v.abs()
    } else {
        let abs = v.abs();
        let bits = abs.to_bits();
        let next = f64::from_bits(bits + 1);
        if next.is_infinite() {
            // At MAX_VALUE the forward step lands on +Infinity, and
            // `Infinity - MAX_VALUE` is Infinity — so the naive
            // `nextUp(x) - x` answered +Infinity where HotSpot answers
            // 2^971 (0x7ca0000000000000). In the top binade the ulp is the
            // BACKWARD step, which is exact and representable.
            abs - f64::from_bits(bits - 1)
        } else {
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
    // See `native_math_ulp_double`: `Math.abs(d)` is the JDK's answer for NaN
    // and for both infinities, and it keeps a NaN payload.
    //
    // EXHAUSTIVELY VERIFIED, this width: all 4,294,967,296 `float` bit patterns
    // were run through this algorithm and through `Math.ulp` on HotSpot 25 and
    // compared as `floatToRawIntBits`. Every non-NaN pattern — 4,278,190,082 of
    // them — already matched; the 16,777,212 that did not were exactly the
    // non-canonical NaNs this change fixes.
    let result = if v.is_nan() || v.is_infinite() {
        v.abs()
    } else {
        let abs = v.abs();
        let bits = abs.to_bits();
        let next = f32::from_bits(bits + 1);
        if next.is_infinite() {
            // See `native_math_ulp_double`: at MAX_VALUE the ulp is the
            // backward step. HotSpot answers 2^104 (0x73800000) here.
            abs - f32::from_bits(bits - 1)
        } else {
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
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let result = if biased == 0x7FF {
        // NaN or Infinity → MAX_EXPONENT + 1
        1024
    } else if biased == 0 {
        if (bits & 0x000F_FFFF_FFFF_FFFF) == 0 {
            // zero → MIN_EXPONENT - 1
            -1023
        } else {
            // subnormal: count leading zeros of significand
            let sig = bits & 0x000F_FFFF_FFFF_FFFF;
            let lz = sig.leading_zeros() as i32 - 12; // 12 bits for sign+exponent
            -1023 - lz
        }
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

static INTEGER_CACHE: std::sync::OnceLock<parking_lot::Mutex<ScopedValueCache<256>>> =
    std::sync::OnceLock::new();

fn integer_cache() -> &'static parking_lot::Mutex<ScopedValueCache<256>> {
    INTEGER_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

static BOOLEAN_CACHE: std::sync::OnceLock<parking_lot::Mutex<ScopedValueCache<2>>> =
    std::sync::OnceLock::new();

fn boolean_cache() -> &'static parking_lot::Mutex<ScopedValueCache<2>> {
    BOOLEAN_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// `LongCache` for `Long.valueOf(long)` — JLS §5.1.7 mandates the canonical
/// cached instance for values in [-128, 127] so `Long.valueOf(x) ==
/// Long.valueOf(x)` holds. Shared across threads but VM-scoped and GC-scanned
/// for the same reasons documented above for `INTEGER_CACHE`.
static LONG_CACHE: std::sync::OnceLock<parking_lot::Mutex<ScopedValueCache<256>>> =
    std::sync::OnceLock::new();

fn long_cache() -> &'static parking_lot::Mutex<ScopedValueCache<256>> {
    LONG_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// GC root scan hook — called from `vm/src/memory/roots.rs::collect_roots`.
/// Reports cached wrapper ObjectRefs for the active VM so the GC keeps them live.
pub fn gc_scan_value_of_cache_roots(vm_identity: usize, out: &mut Vec<cratonvm_types::ObjectRef>) {
    {
        let cache = integer_cache().lock();
        if let Some(entries) = cache.get(&vm_identity) {
            for slot in entries.iter() {
                if let Some(o) = slot {
                    out.push(*o);
                }
            }
        }
    }
    {
        let cache = boolean_cache().lock();
        if let Some(entries) = cache.get(&vm_identity) {
            for slot in entries.iter() {
                if let Some(o) = slot {
                    out.push(*o);
                }
            }
        }
    }
    {
        let cache = long_cache().lock();
        if let Some(entries) = cache.get(&vm_identity) {
            for slot in entries.iter() {
                if let Some(o) = slot {
                    out.push(*o);
                }
            }
        }
    }
}

/// GC post-compaction hook — called from `vm/src/memory/gc.rs::update_all_roots`.
/// Remaps every cached entry for the active VM through the GC's pointer map.
pub fn gc_update_value_of_cache_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    {
        let mut cache = integer_cache().lock();
        if let Some(entries) = cache.get_mut(&vm_identity) {
            for slot in entries.iter_mut() {
                if let Some(obj_ref) = slot {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = pointer_map.get(&old_addr) {
                        debug_assert!(new_addr != 0, "GC pointer map contains null address");
                        *obj_ref =
                            unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
            }
        }
    }
    {
        let mut cache = boolean_cache().lock();
        if let Some(entries) = cache.get_mut(&vm_identity) {
            for slot in entries.iter_mut() {
                if let Some(obj_ref) = slot {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = pointer_map.get(&old_addr) {
                        debug_assert!(new_addr != 0, "GC pointer map contains null address");
                        *obj_ref =
                            unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
            }
        }
    }
    {
        let mut cache = long_cache().lock();
        if let Some(entries) = cache.get_mut(&vm_identity) {
            for slot in entries.iter_mut() {
                if let Some(obj_ref) = slot {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = pointer_map.get(&old_addr) {
                        debug_assert!(new_addr != 0, "GC pointer map contains null address");
                        *obj_ref =
                            unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
            }
        }
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
    if (-128..=127).contains(&val) {
        let idx = (val + 128) as usize;
        let scope = ctx.vm_identity();
        // Fast path: lock, read, drop lock before any heap allocation.
        if let Some(cached) = {
            let c = integer_cache().lock();
            c.get(&scope).and_then(|entries| entries[idx])
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
        let entries = cache.entry(scope).or_insert([None; 256]);
        if let Some(existing) = entries[idx] {
            return Ok(Some(Value::Object(Some(existing))));
        }
        entries[idx] = Some(obj);
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

pub(crate) fn native_character_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
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

/// `Character.isUpperCase` / `isLowerCase` are the Unicode **Uppercase** and
/// **Lowercase** properties — `Lu u Other_Uppercase` and `Ll u Other_Lowercase`
/// — which is exactly what `char::is_uppercase`/`is_lowercase` implement, so
/// unlike `isLetter` these two are NOT a definitional mismatch. Verified on
/// JDK 25: `isUpperCase` differs from `getType == UPPERCASE_LETTER` on 120 code
/// points and `isLowerCase` from `LOWERCASE_LETTER` on 311, i.e. Java really
/// does take the contributory properties.
///
/// W7-95(C1). What remained after that was six code points of TOOLCHAIN
/// version skew, measured by sweeping all 65,536 BMP code points on both VMs:
/// `U+A7CE`, `U+A7D2`, `U+A7D4` answered uppercase here and do not on JDK 25;
/// `U+A7CF` and `U+A7F1` answered lowercase here and do not; `U+0295` LATIN
/// LETTER PHARYNGEAL VOICED FRICATIVE answers lowercase on JDK 25 and did not
/// here. Same cause as [`JAVA_SIMPLE_LOWERCASE_OVERRIDES`]'s Latin Extended-D
/// entries. Pinned rather than left open, because six is small enough to
/// vanish from a residual list and large enough to fail a differential.
///
/// Five of the six are one fact, and it is checkable rather than asserted:
/// `Character.getType` on HotSpot 25 answers `0` (`UNASSIGNED`) for `U+A7CE`,
/// `U+A7CF`, `U+A7D2`, `U+A7D4` and `U+A7F1`. An unassigned code point is not
/// uppercase, not lowercase, and maps to itself — so one rule covers all five
/// and both predicates, rather than five ad-hoc entries. (`U+A7D3` and `U+A7D5`
/// ARE assigned on JDK 25, `getType == LOWERCASE_LETTER`; only their spurious
/// uppercase MAPPING needed correcting, which
/// [`JAVA_SIMPLE_UPPERCASE_OVERRIDES`] does.) The sixth, `U+0295`, is the
/// reverse direction: JDK 25 has it as `LOWERCASE_LETTER` and the toolchain
/// does not.
const JAVA_UNASSIGNED_ON_JDK25: &[u32] = &[0xA7CE, 0xA7CF, 0xA7D2, 0xA7D4, 0xA7F1];

pub(crate) fn native_character_is_upper_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = !JAVA_UNASSIGNED_ON_JDK25.contains(&ch)
        && char::from_u32(ch).is_some_and(|c| c.is_uppercase());
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
    let result = ch == 0x0295
        || (!JAVA_UNASSIGNED_ON_JDK25.contains(&ch)
            && char::from_u32(ch).is_some_and(|c| c.is_lowercase()));
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// A lone surrogate is a legal `char` value; it must survive a case mapping
/// unchanged.
///
/// W7-98(c), and the highest-severity defect in this family because it is
/// **silent data corruption**, not a wrong classification. `char::from_u32`
/// answers `None` for `U+D800..U+DFFF` (they are not Unicode scalar values, so
/// Rust's `char` cannot hold one), and every body here used to finish with
/// `.unwrap_or('\0')` — so `Character.toUpperCase('\uD800')` answered `U+0000`
/// where HotSpot 25 answers `'\uD800'`. Measured on all six of
/// `U+D800/U+D83D/U+DBFF/U+DC00/U+DE00/U+DFFF`, for both `toUpperCase(C)C` and
/// `toLowerCase(C)C`: HotSpot returns the input, CratonVM returned 0. A split
/// UTF-16 pair — the normal state of a `char` halfway through a surrogate pair,
/// and of any text chunked on a non-code-point boundary — was being zeroed.
///
/// The `int`-taking overloads already had the right shape (`.unwrap_or(cp)`);
/// this makes the `char` overloads agree.
/// The BMP code points where JDK 25's SIMPLE case mapping and the answer this
/// file could otherwise produce disagree — measured, not derived.
///
/// W7-95(C1). Both tables come from diffing a full 65,536-code-point sweep of
/// `Character.toUpperCase`/`toLowerCase` run on the CratonVM binary against the
/// same sweep on HotSpot 25, so every entry is an executed divergence rather
/// than a guess about what Rust's tables contain. There are exactly two causes,
/// and they are worth keeping distinct:
///
/// 1. **The ypogegrammeni family**, 27 of the 30 upper rows
///    (`U+1F80..U+1F87`, `U+1F90..U+1F97`, `U+1FA0..U+1FA7`, `U+1FB3`,
///    `U+1FC3`, `U+1FF3`). These have a multi-char FULL uppercase *and* a
///    single-char SIMPLE uppercase (`U+1FB3` -> `U+1FBC`), and Rust's std
///    exposes only the full mapping — so the arity rule below correctly refuses
///    it and returns the input, where Java returns the titlecase form. W7-98
///    predicted this exact residual by name and could not close it; the
///    enumeration closes it.
/// 2. **A toolchain-vs-JDK Unicode version skew** in Latin Extended-D:
///    `U+A7CE/A7CF`, `U+A7D2/A7D3`, `U+A7D4/A7D5`. The toolchain's tables carry
///    these as case PAIRS; JDK 25 maps each to itself. This is not a
///    specification difference — it is two Unicode versions — and it is the
///    only part of this file that pins one.
#[rustfmt::skip]
const JAVA_SIMPLE_UPPERCASE_OVERRIDES: &[(u32, u32)] = &[
    (0x1F80, 0x1F88), (0x1F81, 0x1F89), (0x1F82, 0x1F8A), (0x1F83, 0x1F8B), (0x1F84, 0x1F8C),
    (0x1F85, 0x1F8D), (0x1F86, 0x1F8E), (0x1F87, 0x1F8F), (0x1F90, 0x1F98), (0x1F91, 0x1F99),
    (0x1F92, 0x1F9A), (0x1F93, 0x1F9B), (0x1F94, 0x1F9C), (0x1F95, 0x1F9D), (0x1F96, 0x1F9E),
    (0x1F97, 0x1F9F), (0x1FA0, 0x1FA8), (0x1FA1, 0x1FA9), (0x1FA2, 0x1FAA), (0x1FA3, 0x1FAB),
    (0x1FA4, 0x1FAC), (0x1FA5, 0x1FAD), (0x1FA6, 0x1FAE), (0x1FA7, 0x1FAF), (0x1FB3, 0x1FBC),
    (0x1FC3, 0x1FCC), (0x1FF3, 0x1FFC), (0xA7CF, 0xA7CF), (0xA7D3, 0xA7D3), (0xA7D5, 0xA7D5),
];

/// See [`JAVA_SIMPLE_UPPERCASE_OVERRIDES`]. All three are cause (2).
#[rustfmt::skip]
const JAVA_SIMPLE_LOWERCASE_OVERRIDES: &[(u32, u32)] = &[
    (0xA7CE, 0xA7CE), (0xA7D2, 0xA7D2), (0xA7D4, 0xA7D4),
];

#[inline]
fn case_override(table: &[(u32, u32)], cp: u32) -> Option<u32> {
    table
        .binary_search_by_key(&cp, |&(from, _)| from)
        .ok()
        .map(|i| table[i].1)
}

#[inline]
fn character_case_map(ch: u32, upper: bool) -> u32 {
    if let Some(mapped) = case_override(
        if upper {
            JAVA_SIMPLE_UPPERCASE_OVERRIDES
        } else {
            JAVA_SIMPLE_LOWERCASE_OVERRIDES
        },
        ch,
    ) {
        return mapped;
    }
    let Some(c) = char::from_u32(ch) else {
        // Surrogate (or otherwise not a scalar value): return it unchanged.
        return ch;
    };
    if upper {
        // W7-98(b). Rust exposes the Unicode **full** uppercase mapping
        // (`SpecialCasing.txt`), which is an ITERATOR; Java's
        // `Character.toUpperCase` is the **simple** mapping
        // (`UnicodeData.txt` field 12), which is one code point or none.
        // Taking `.next()` silently returned the first char of a multi-char
        // full mapping: measured against HotSpot 25, `toUpperCase('ß')`
        // answered `'S'` (Java: `'ß'`), `'ﬀ'` answered `'F'` (Java: `'ﬀ'`),
        // and the same for `U+FB01/U+FB02/U+FB03/U+FB05/U+0149/U+01F0/
        // U+0390/U+03B0/U+1E96/U+1F50` — eleven code points where the correct
        // answer is "unchanged, because the uppercase does not fit in a
        // `char`". Requiring the mapping to be exactly one char restores all
        // eleven.
        //
        // NOT exact, and deliberately not claimed to be: `U+1FB3` and its
        // ypogegrammeni family have a multi-char FULL mapping *and* a
        // single-char SIMPLE mapping (`U+1FB3` -> `U+1FBC`), which no Rust
        // std API exposes. That code point stays wrong — but wrong as the
        // IDENTITY rather than as a different letter (`U+0391`), which is the
        // safer of the two failures. The exact fix is to stop shadowing the
        // JDK bytecode at all; see docs/known-issues/jdk-only/W7-98.
        let mut it = c.to_uppercase();
        match (it.next(), it.next()) {
            (Some(u), None) => u as u32,
            _ => ch,
        }
    } else {
        // Lowercase deliberately keeps `.next()`. The arity rule is NOT
        // symmetric here and applying it would REGRESS a currently-correct
        // row: `U+0130` (LATIN CAPITAL LETTER I WITH DOT ABOVE) has a
        // two-char full lowercase (`i` + `U+0307`) whose FIRST char is
        // exactly Java's simple mapping, so HotSpot and CratonVM both answer
        // `105` today. Measured: no `toLowerCase` row in the 755-row
        // differential census diverges except the surrogates fixed above.
        c.to_lowercase().next().map_or(ch, |l| l as u32)
    }
}

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
    Ok(Some(Value::Int(character_case_map(cp as u32, false) as i32)))
}

/// `Character.toUpperCase(int)` — code-point variant, mirror of
/// `toLowerCase(I)I`.
///
/// W7-98(b): shares [`character_case_map`] with the `(C)C` form, so the
/// simple-vs-full uppercase-mapping fix applies to both overloads. Before the
/// fix this overload disagreed with HotSpot on the same eleven code points
/// (`U+00DF`, `U+FB00..U+FB05`, `U+0149`, `U+01F0`, `U+0390`, `U+03B0`,
/// `U+1E96`, `U+1F50`) as the `char` form.
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
        Value::Int(v) => v as u32,
        _ => 0,
    };
    let ch = char::from_u32(val).unwrap_or('\0');
    let s = ctx.create_string(&ch.to_string());
    Ok(Some(Value::Object(Some(s))))
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
    Ok(Some(Value::Int(val.to_bits() as i32)))
}

// hashCode for Double wrapper: bits = doubleToLongBits; (bits ^ (bits >>> 32)) as i32
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
    let bits = val.to_bits() as i64;
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
    Ok(Some(Value::Int(if a.to_bits() == b.to_bits() {
        1
    } else {
        0
    })))
}

// equals for Double wrapper (NaN == NaN is true per Double.equals spec, using to_bits)
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
    Ok(Some(Value::Int(if a.to_bits() == b.to_bits() {
        1
    } else {
        0
    })))
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

pub(crate) fn native_character_char_count(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if cp > 0xFFFF { 2 } else { 1 })))
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
    Word { nan: bool, neg: bool },
    /// A numeric body with its sign, suffix already removed.
    Body { body: &'a str, neg: bool },
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
        let low = if sh >= 128 { m } else { m & ((1u128 << sh) - 1) };
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
        _ => Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        ),
    }
}

fn parse_float_string(s: &str) -> Result<f32, cratonvm_types::error::RuntimeError> {
    let (body, neg) = match java_float_head(s) {
        JavaFloatHead::Malformed => return Err(java_nfe_float(s)),
        JavaFloatHead::Word { nan: true, .. } => return Ok(f32::NAN),
        JavaFloatHead::Word { nan: false, neg } => {
            return Ok(if neg { f32::NEG_INFINITY } else { f32::INFINITY })
        }
        JavaFloatHead::Body { body, neg } => (body, neg),
    };
    let is_hex = body.len() > 1
        && body.as_bytes()[0] == b'0'
        && (body.as_bytes()[1] == b'x' || body.as_bytes()[1] == b'X');
    let v = if is_hex {
        let (digits, frac_n, pexp) = java_hex_grammar(&body[2..]).ok_or_else(|| java_nfe_float(s))?;
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

fn parse_double_string(s: &str) -> Result<f64, cratonvm_types::error::RuntimeError> {
    let (body, neg) = match java_float_head(s) {
        JavaFloatHead::Malformed => return Err(java_nfe_float(s)),
        JavaFloatHead::Word { nan: true, .. } => return Ok(f64::NAN),
        JavaFloatHead::Word { nan: false, neg } => {
            return Ok(if neg { f64::NEG_INFINITY } else { f64::INFINITY })
        }
        JavaFloatHead::Body { body, neg } => (body, neg),
    };
    let is_hex = body.len() > 1
        && body.as_bytes()[0] == b'0'
        && (body.as_bytes()[1] == b'x' || body.as_bytes()[1] == b'X');
    let v = if is_hex {
        let (digits, frac_n, pexp) = java_hex_grammar(&body[2..]).ok_or_else(|| java_nfe_float(s))?;
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
    // total_cmp matches Java semantics: -0.0 < +0.0, NaN > everything
    Ok(Some(Value::Int(a.total_cmp(&b) as i32)))
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
    Ok(Some(Value::Int(a.total_cmp(&b) as i32)))
}

// --- Byte ---

pub(crate) fn native_byte_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Byte");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

// --- Short ---

pub(crate) fn native_short_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::mock_ctx;

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
        // f64::max(1.0, NaN) returns 1.0 in Rust (propagates non-NaN)
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
            let ok = [2i32, 10, 36]
                .iter()
                .all(|&r| {
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
        assert!(ok, "legal radices must still parse (and base 2 must reject \"5\")");
        worker.join().expect("worker thread panicked");
    }
}
