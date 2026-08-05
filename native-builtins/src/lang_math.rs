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
    registry.register(class, "sqrt", "(D)D", native_math_sqrt);
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
    registry.register(class, "cbrt", "(D)D", native_math_cbrt);
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
    registry.register(class, "hypot", "(DD)D", native_math_hypot);
    registry.register(class, "log1p", "(D)D", native_math_log1p);
    registry.register(class, "expm1", "(D)D", native_math_expm1);
    registry.register(class, "sinh", "(D)D", native_math_sinh);
    registry.register(class, "cosh", "(D)D", native_math_cosh);
    registry.register(class, "tanh", "(D)D", native_math_tanh);
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
        native_string_chars, // Same as chars for BMP
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
    // Java 21: Character emoji detection methods
    registry.register("java/lang/Character", "isEmoji", "(I)Z", |_ctx, args| {
        let cp = match args.first() {
            Some(Value::Int(v)) => *v as u32,
            _ => 0,
        };
        // Basic emoji ranges: emoticons, transport, misc symbols, dingbats, regional indicators
        let is_emoji = matches!(cp,
            0x231A..=0x231B | 0x23E9..=0x23F3 | 0x23F8..=0x23FA |
            0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FE |
            0x2600..=0x27BF | 0x2934..=0x2935 | 0x2B05..=0x2B07 |
            0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x3030 | 0x303D |
            0x3297 | 0x3299 | 0x1F004 | 0x1F0CF |
            0x1F170..=0x1F171 | 0x1F17E..=0x1F17F | 0x1F18E |
            0x1F191..=0x1F19A | 0x1F1E0..=0x1F1FF |
            0x1F200..=0x1F251 | 0x1F300..=0x1F9FF |
            0x1FA00..=0x1FA6F | 0x1FA70..=0x1FAFF |
            0x200D | 0xFE0F | 0x20E3 |
            0x0023 | 0x002A | 0x0030..=0x0039
        );
        Ok(Some(Value::Int(if is_emoji { 1 } else { 0 })))
    });
    registry.register("java/lang/Character", "isEmojiPresentation", "(I)Z", |_ctx, args| {
        let cp = match args.first() { Some(Value::Int(v)) => *v as u32, _ => 0 };
        let is_ep = matches!(cp, 0x1F300..=0x1F9FF | 0x1FA00..=0x1FAFF | 0x2600..=0x26FF | 0x2700..=0x27BF);
        Ok(Some(Value::Int(if is_ep { 1 } else { 0 })))
    });
    registry.register(
        "java/lang/Character",
        "isEmojiModifier",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_em = (0x1F3FB..=0x1F3FF).contains(&cp);
            Ok(Some(Value::Int(if is_em { 1 } else { 0 })))
        },
    );
    registry.register("java/lang/Character", "isEmojiModifierBase", "(I)Z", |_ctx, args| {
        let cp = match args.first() { Some(Value::Int(v)) => *v as u32, _ => 0 };
        let is_emb = matches!(cp, 0x261D | 0x26F9 | 0x270A..=0x270D | 0x1F385 | 0x1F3C2..=0x1F3C4 |
            0x1F3C7 | 0x1F3CA..=0x1F3CC | 0x1F442..=0x1F443 | 0x1F446..=0x1F450 |
            0x1F466..=0x1F478 | 0x1F47C | 0x1F481..=0x1F483 | 0x1F485..=0x1F487 |
            0x1F4AA | 0x1F574..=0x1F575 | 0x1F57A | 0x1F590 | 0x1F595..=0x1F596 |
            0x1F645..=0x1F647 | 0x1F64B..=0x1F64F | 0x1F6A3 | 0x1F6B4..=0x1F6B6 |
            0x1F6C0 | 0x1F6CC | 0x1F90F | 0x1F918..=0x1F91F | 0x1F926 |
            0x1F930..=0x1F939 | 0x1F93D..=0x1F93E | 0x1F9B5..=0x1F9B6 | 0x1F9B8..=0x1F9B9 |
            0x1F9BB | 0x1F9CD..=0x1F9CF | 0x1F9D1..=0x1F9DD
        );
        Ok(Some(Value::Int(if is_emb { 1 } else { 0 })))
    });
    registry.register(
        "java/lang/Character",
        "isEmojiComponent",
        "(I)Z",
        |_ctx, args| {
            let cp = match args.first() {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let is_ec = matches!(cp, 0x200D | 0xFE0E..=0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF |
            0xE0020..=0xE007F | 0x0023 | 0x002A | 0x0030..=0x0039);
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
    Ok(Some(Value::Float(a.max(b))))
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
    Ok(Some(Value::Double(a.max(b))))
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
    Ok(Some(Value::Float(a.min(b))))
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
    Ok(Some(Value::Double(a.min(b))))
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
    // HotSpot-style fast path: integer-valued exponent with finite base.
    // - Gated on a.is_finite() && b.is_finite() so NaN/±infinity edge cases fall through to powf,
    //   preserving Java/JLS special-value semantics (e.g. pow(NaN, 0) == 1, pow(±0, neg) == ±inf,
    //   pow(1, ±inf) == NaN per JLS, etc.).
    // - b.fract() == 0.0 ensures b is an exact integer (also false for NaN, but we already gated that).
    // - |b| < 64 keeps powi cheap and avoids producing values that overflow to ±inf when powf
    //   would have given a finite (but huge) result via continuous exponentiation.
    // - Negative bases with integer exponents are fine: powi does repeated multiplication, which
    //   matches Java's result for integer-valued b. Only fractional b on negative a yields NaN in
    //   Java, and we route those through powf.
    if a.is_finite() && b.is_finite() && b.fract() == 0.0 && b.abs() < 64.0 {
        let bi = b as i32;
        return Ok(Some(Value::Double(a.powi(bi))));
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
    // IEEE 754 remainder — same as Rust's f64 rem_euclid? No, it's f64::rem.
    // Actually Java IEEEremainder = a - (round(a/b) * b)
    let result = if b == 0.0 || a.is_infinite() || b.is_nan() {
        f64::NAN
    } else {
        let q = (a / b).round();
        a - q * b
    };
    Ok(Some(Value::Double(result)))
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
    // Java floorDiv: rounds toward negative infinity
    let d = a / b;
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d };
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
    let d = a / b;
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d };
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
    // Java floorMod: a - floorDiv(a,b) * b
    let r = a % b;
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
    let r = a % b;
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
    let result = if v.is_nan() {
        f64::NAN
    } else if v.is_infinite() {
        f64::INFINITY
    } else {
        let abs = v.abs();
        let next = f64::from_bits(abs.to_bits() + 1);
        next - abs
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
    let result = if v.is_nan() {
        f32::NAN
    } else if v.is_infinite() {
        f32::INFINITY
    } else {
        let abs = v.abs();
        let next = f32::from_bits(abs.to_bits() + 1);
        next - abs
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
    pointer_map: &std::collections::HashMap<usize, usize>,
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

pub(crate) fn native_integer_parse_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i32>() {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_integer_parse_int_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match i32::from_str_radix(text.trim(), radix) {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

// --- Byte.parseByte ---

pub(crate) fn native_byte_parse_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i8>() {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_byte_parse_byte_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match i8::from_str_radix(text.trim(), radix) {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

// --- Short.parseShort ---

pub(crate) fn native_short_parse_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i16>() {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_short_parse_short_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match i16::from_str_radix(text.trim(), radix) {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
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
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i64>() {
        Ok(v) => Ok(Some(Value::Long(v))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
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

pub(crate) fn native_character_is_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_ascii_digit());
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_is_letter(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_alphabetic());
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_is_whitespace(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_whitespace());
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_is_upper_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_uppercase());
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
    let result = char::from_u32(ch).is_some_and(|c| c.is_lowercase());
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_to_upper_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch)
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('\0') as u32;
    Ok(Some(Value::Int(result as i32)))
}

pub(crate) fn native_character_to_lower_case(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch)
        .and_then(|c| c.to_lowercase().next())
        .unwrap_or('\0') as u32;
    Ok(Some(Value::Int(result as i32)))
}

/// `Character.toLowerCase(int)` — code-point variant. Fall-through to the
/// same Rust `char::to_lowercase` for valid scalar values; pass invalid /
/// out-of-range code points back unchanged (matching JDK behaviour for
/// non-character integers).
pub(crate) fn native_character_to_lower_case_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let result = char::from_u32(cp as u32)
        .and_then(|c| c.to_lowercase().next())
        .map(|c| c as u32 as i32)
        .unwrap_or(cp);
    Ok(Some(Value::Int(result)))
}

/// `Character.toUpperCase(int)` — code-point variant, mirror of
/// `toLowerCase(I)I` to keep the JIT-bypass symmetric (the same compile
/// path that miscompiles the lowercase chain miscompiles the uppercase
/// chain — register both pre-emptively).
pub(crate) fn native_character_to_upper_case_int(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let cp = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let result = char::from_u32(cp as u32)
        .and_then(|c| c.to_uppercase().next())
        .map(|c| c as u32 as i32)
        .unwrap_or(cp);
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_character_is_letter_or_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_alphanumeric());
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

pub(crate) fn native_character_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let result = char::from_u32(ch)
        .and_then(|c| c.to_digit(radix))
        .map(|d| d as i32)
        .unwrap_or(-1);
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_character_for_digit(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let digit = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(0))),
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let result = char::from_digit(digit, radix).unwrap_or('\0') as i32;
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_character_get_numeric_value(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let result = char::from_u32(ch)
        .and_then(|c| c.to_digit(36))
        .map(|d| d as i32)
        .unwrap_or(-1);
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

pub(crate) fn native_character_static_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let c = char::from_u32(ch).unwrap_or('\0');
    let s = ctx.create_string(&c.to_string());
    Ok(Some(Value::Object(Some(s))))
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

pub(crate) fn native_integer_to_string_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = if radix == 10 {
        val.to_string()
    } else {
        // Handle negative numbers: Java uses "-" prefix for negatives
        if val < 0 {
            format!("-{}", i64_to_radix_string(-(val as i64), radix))
        } else {
            i64_to_radix_string(val as i64, radix)
        }
    };
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

pub(crate) fn native_long_parse_long_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match i64::from_str_radix(text.trim(), radix) {
        Ok(v) => Ok(Some(Value::Long(v))),
        Err(_) => Err(cratonvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_long_to_string_radix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let radix = match args.get(1) {
        Some(Value::Int(v)) => *v as u32,
        _ => 10,
    };
    let text = if radix == 10 {
        val.to_string()
    } else if val < 0 {
        // NB: negating val overflows (panics in debug builds) when
        // val == i64::MIN, since i64::MIN has no positive counterpart
        // in twos-complement. Using the wrapping negation is safe
        // here: for every val != i64::MIN it equals plain negation,
        // and for val == i64::MIN it wraps back to i64::MIN, whose
        // unsigned bit pattern (i64_to_radix_string reinterprets its
        // arg as u64) is exactly 2 to the 63 -- the correct unsigned
        // magnitude of i64::MIN.
        format!("-{}", i64_to_radix_string(val.wrapping_neg(), radix))
    } else {
        i64_to_radix_string(val, radix)
    };
    let result = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(result))))
}

/// Convert a non-negative i64 to a string in the given radix.
fn i64_to_radix_string(val: i64, radix: u32) -> String {
    if val == 0 {
        return "0".to_string();
    }
    // Radix digits are always ASCII, so build the byte buffer directly and
    // construct the String once (avoids the intermediate Vec<char> + collect).
    let mut v = val as u64;
    let mut digits: Vec<u8> = Vec::new();
    while v > 0 {
        let d = (v % radix as u64) as u32;
        digits.push(char::from_digit(d, radix).unwrap_or('?') as u8);
        v /= radix as u64;
    }
    digits.reverse();
    // Every byte pushed is an ASCII digit/letter from `char::from_digit`
    // (or the ASCII `?` fallback), so the buffer is guaranteed valid UTF-8.
    String::from_utf8(digits).unwrap_or_else(|_| "0".to_string())
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

fn strip_java_float_type_suffix(s: &str) -> &str {
    let Some(&suffix) = s.as_bytes().last() else {
        return s;
    };
    if !matches!(suffix, b'd' | b'D' | b'f' | b'F') {
        return s;
    }

    let numeric = &s[..s.len() - 1];
    let has_digit = numeric.bytes().any(|b| b.is_ascii_digit());
    let suffix_follows_number = matches!(
        numeric.as_bytes().last().copied(),
        Some(b'0'..=b'9') | Some(b'.')
    );
    if has_digit && suffix_follows_number {
        numeric
    } else {
        s
    }
}

fn parse_float_string(s: &str) -> Result<f32, cratonvm_types::error::RuntimeError> {
    let trimmed = s.trim();
    let numeric = strip_java_float_type_suffix(trimmed);
    match numeric {
        "NaN" => Ok(f32::NAN),
        "Infinity" | "+Infinity" => Ok(f32::INFINITY),
        "-Infinity" => Ok(f32::NEG_INFINITY),
        _ => numeric.parse::<f32>().map_err(|_| {
            cratonvm_types::error::RuntimeError::NumberFormatException {
                message: format!("For input string: \"{s}\""),
            }
        }),
    }
}

fn parse_double_string(s: &str) -> Result<f64, cratonvm_types::error::RuntimeError> {
    let trimmed = s.trim();
    let numeric = strip_java_float_type_suffix(trimmed);
    match numeric {
        "NaN" => Ok(f64::NAN),
        "Infinity" | "+Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        _ => numeric.parse::<f64>().map_err(|_| {
            cratonvm_types::error::RuntimeError::NumberFormatException {
                message: format!("For input string: \"{s}\""),
            }
        }),
    }
}
pub(crate) fn native_float_parse_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let val = parse_float_string(&s)?;
    Ok(Some(Value::Float(val)))
}

pub(crate) fn native_double_parse_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let val = parse_double_string(&s)?;
    Ok(Some(Value::Double(val)))
}

pub(crate) fn native_float_value_of_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let val = parse_float_string(&s)?;
    let obj = alloc_wrapper(ctx, "java/lang/Float");
    ctx.set_field(obj, 0, Value::Float(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_double_value_of_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
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
                assert!((v - std::f64::consts::PI).abs() < 1e-10);
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
                assert!((v - 180.0).abs() < 1e-10);
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
