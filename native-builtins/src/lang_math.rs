//! Math, StrictMath, and Number subclass native method implementations.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::Value;
use rustjvm_types::error::MethodCallResult;

use crate::lang_string::{
    format_double, format_float, native_string_chars, native_string_code_point_at,
    native_string_code_point_count, native_string_format, native_string_format_locale,
    native_string_formatted,
    native_string_indent, native_string_is_blank, native_string_lines,
    native_string_offset_by_code_points, native_string_region_matches,
    native_string_region_matches_ic, native_string_repeat, native_string_transform,
    native_string_value_of_int, native_string_value_of_long, native_string_value_of_object,
};

pub(crate) fn register_math_natives(registry: &mut NativeMethodRegistry, class: &str) {
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
}

pub(crate) fn register_wrapper_natives(registry: &mut NativeMethodRegistry) {
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
        "isLetter",
        "(C)Z",
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
        "isUpperCase",
        "(C)Z",
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
    registry.register(
        "java/lang/Character",
        "isLetterOrDigit",
        "(C)Z",
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
}

// ---------------------------------------------------------------------------
// java.lang.Float / Double natives
// ---------------------------------------------------------------------------

pub(crate) fn native_float_to_raw_int_bits(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_long_bits_to_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_math_abs_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    Ok(Some(Value::Int(v.wrapping_abs())))
}

pub(crate) fn native_math_abs_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = long_arg(args, 0);
    Ok(Some(Value::Long(v.wrapping_abs())))
}

pub(crate) fn native_math_abs_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Float(v.abs())))
}

pub(crate) fn native_math_abs_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.abs())))
}

// --- max ---
pub(crate) fn native_math_max_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_max_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = long_arg(args, 0);
    let b = long_arg(args, 1);
    Ok(Some(Value::Long(std::cmp::max(a, b))))
}

pub(crate) fn native_math_max_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_max_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_math_min_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_min_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = long_arg(args, 0);
    let b = long_arg(args, 1);
    Ok(Some(Value::Long(std::cmp::min(a, b))))
}

pub(crate) fn native_math_min_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_min_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_math_sqrt(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sqrt())))
}

pub(crate) fn native_math_pow(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let b = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(a.powf(b))))
}

pub(crate) fn native_math_sin(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sin())))
}

pub(crate) fn native_math_cos(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cos())))
}

pub(crate) fn native_math_tan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.tan())))
}

pub(crate) fn native_math_asin(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.asin())))
}

pub(crate) fn native_math_acos(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.acos())))
}

pub(crate) fn native_math_atan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.atan())))
}

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

pub(crate) fn native_math_log(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ln())))
}

pub(crate) fn native_math_log10(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.log10())))
}

pub(crate) fn native_math_exp(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.exp())))
}

pub(crate) fn native_math_floor(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.floor())))
}

pub(crate) fn native_math_ceil(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ceil())))
}

pub(crate) fn native_math_rint(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Java Math.rint: round to nearest even (banker's rounding)
    Ok(Some(Value::Double(v.round_ties_even())))
}

pub(crate) fn native_math_round_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    // Java Math.round(double) returns long, uses floor(v + 0.5)
    if v.is_nan() {
        Ok(Some(Value::Long(0)))
    } else if v >= i64::MAX as f64 {
        Ok(Some(Value::Long(i64::MAX)))
    } else if v <= i64::MIN as f64 {
        Ok(Some(Value::Long(i64::MIN)))
    } else {
        Ok(Some(Value::Long((v + 0.5).floor() as i64)))
    }
}

pub(crate) fn native_math_round_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    // Java Math.round(float) returns int, uses floor(v + 0.5)
    if v.is_nan() {
        Ok(Some(Value::Int(0)))
    } else if v >= i32::MAX as f32 {
        Ok(Some(Value::Int(i32::MAX)))
    } else if v <= i32::MIN as f32 {
        Ok(Some(Value::Int(i32::MIN)))
    } else {
        Ok(Some(Value::Int((v + 0.5f32).floor() as i32)))
    }
}

pub(crate) fn native_math_to_radians(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.to_radians())))
}

pub(crate) fn native_math_to_degrees(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.to_degrees())))
}

pub(crate) fn native_math_random(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: AtomicU64 = AtomicU64::new(0x5DEECE66D);

    // Simple LCG (same constants as java.util.Random)
    let old = SEED.load(Ordering::Relaxed);
    let new_seed = old.wrapping_mul(0x5DEECE66D).wrapping_add(0xB) & 0xFFFF_FFFF_FFFF;
    SEED.store(new_seed, Ordering::Relaxed);

    // Use top 53 bits to make a double in [0, 1)
    let bits = (new_seed >> 1) as f64 / (1u64 << 47) as f64;
    Ok(Some(Value::Double(bits.abs().fract())))
}

pub(crate) fn native_math_signum_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_signum_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_cbrt(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cbrt())))
}

pub(crate) fn native_math_ieee_remainder(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn math_overflow_err() -> rustjvm_types::error::MethodCallFailed {
    rustjvm_types::error::RuntimeError::ArithmeticException {
        message: "integer overflow".to_string(),
    }
    .into()
}

pub(crate) fn native_math_add_exact_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_add_exact_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        None => Err(math_overflow_err()),
    }
}

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
        None => Err(math_overflow_err()),
    }
}

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
        None => Err(math_overflow_err()),
    }
}

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
        None => Err(math_overflow_err()),
    }
}

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
        None => Err(math_overflow_err()),
    }
}

pub(crate) fn native_math_negate_exact_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match a.checked_neg() {
        Some(r) => Ok(Some(Value::Int(r))),
        None => Err(math_overflow_err()),
    }
}

pub(crate) fn native_math_negate_exact_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match a.checked_neg() {
        Some(r) => Ok(Some(Value::Long(r))),
        None => Err(math_overflow_err()),
    }
}

// ---------------------------------------------------------------------------
// Phase 13 Step 2: Floor/ceil division + toIntExact
// ---------------------------------------------------------------------------

pub(crate) fn native_math_floor_div_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(rustjvm_types::error::RuntimeError::ArithmeticException {
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

pub(crate) fn native_math_floor_div_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(rustjvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    let d = a / b;
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d };
    Ok(Some(Value::Long(result)))
}

pub(crate) fn native_math_floor_mod_int(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(rustjvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    // Java floorMod: a - floorDiv(a,b) * b
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
    Ok(Some(Value::Int(result)))
}

pub(crate) fn native_math_floor_mod_long(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let b = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 1,
    };
    if b == 0 {
        return Err(rustjvm_types::error::RuntimeError::ArithmeticException {
            message: "/ by zero".to_string(),
        }
        .into());
    }
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
    Ok(Some(Value::Long(result)))
}

pub(crate) fn native_math_to_int_exact(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match i32::try_from(v) {
        Ok(i) => Ok(Some(Value::Int(i))),
        Err(_) => Err(math_overflow_err()),
    }
}

pub(crate) fn native_math_multiply_high(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

// ---------------------------------------------------------------------------
// Phase 13 Step 3: Advanced math functions
// ---------------------------------------------------------------------------

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

pub(crate) fn native_math_log1p(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.ln_1p())))
}

pub(crate) fn native_math_expm1(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.exp_m1())))
}

pub(crate) fn native_math_sinh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.sinh())))
}

pub(crate) fn native_math_cosh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.cosh())))
}

pub(crate) fn native_math_tanh(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Double(v.tanh())))
}

pub(crate) fn native_math_copy_sign_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_copy_sign_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_next_up_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_next_down_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_next_after(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_ulp_double(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_math_ulp_float(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

/// Helper: allocate a wrapper object with 1 field using a well-known class name.
/// Falls back to ClassId(0) with 1 field if the class can't be loaded.
pub(crate) fn alloc_wrapper(ctx: &mut dyn NativeContext, class_name: &str) -> rustjvm_types::ObjectRef {
    // Try to load the class; if it fails, use a synthetic object
    match ctx.ensure_class_initialized(class_name) {
        Ok(class_id) => ctx.alloc_object(class_id, 1),
        Err(e) => {
            eprintln!("[alloc_wrapper] Failed to init {}: {:?}", class_name, e);
            ctx.alloc_object(rustjvm_types::ClassId::new(0), 1)
        }
    }
}

pub(crate) fn native_integer_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Integer");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

/// Shared unboxing for Integer.intValue(), Boolean.booleanValue(),
/// Character.charValue(), Byte.byteValue(), Short.shortValue().
pub(crate) fn native_wrapper_int_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_integer_parse_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i32>() {
        Ok(v) => Ok(Some(Value::Int(v))),
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_integer_parse_int_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

// --- Byte.parseByte ---

pub(crate) fn native_byte_parse_byte(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i8>() {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_byte_parse_byte_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

// --- Short.parseShort ---

pub(crate) fn native_short_parse_short(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i16>() {
        Ok(v) => Ok(Some(Value::Int(v as i32))),
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_short_parse_short_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_integer_to_hex_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
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

pub(crate) fn native_integer_bit_count(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.count_ones() as i32)))
}

pub(crate) fn native_integer_reverse(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.reverse_bits() as i32)))
}

pub(crate) fn native_integer_reverse_bytes(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    Ok(Some(Value::Int(v.swap_bytes() as i32)))
}

// --- Long ---

pub(crate) fn native_long_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Long");
    ctx.set_field(obj, 0, Value::Long(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Long(0))),
    };
    let val = ctx.get_field(this, 0);
    match val {
        Value::Long(_) => Ok(Some(val)),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub(crate) fn native_long_parse_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let text = ctx.read_string(s_obj).unwrap_or_default();
    match text.trim().parse::<i64>() {
        Ok(v) => Ok(Some(Value::Long(v))),
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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

pub(crate) fn native_long_bit_count(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Int(v.count_ones() as i32)))
}

pub(crate) fn native_long_reverse(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Long(v.reverse_bits() as i64)))
}

pub(crate) fn native_long_reverse_bytes(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Long(v)) => *v as u64,
        _ => 0,
    };
    Ok(Some(Value::Long(v.swap_bytes() as i64)))
}

// --- Boolean ---

pub(crate) fn native_boolean_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Boolean");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

// --- Character ---

pub(crate) fn native_character_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Character");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_character_is_digit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ch = match args.first() {
        Some(Value::Int(v)) => *v as u32,
        _ => 0,
    };
    let result = char::from_u32(ch).is_some_and(|c| c.is_ascii_digit());
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub(crate) fn native_character_is_letter(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_int_to_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_int_to_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_int_to_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_long_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_long_to_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_long_to_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_float_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_float_to_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_float_to_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_double_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_double_to_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_double_to_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_int_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_int_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_boolean_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_long_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
pub(crate) fn native_wrapper_float_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

// equals for Int-stored wrappers (Integer, Boolean, Character, Byte, Short)
pub(crate) fn native_wrapper_int_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = ctx.get_field(this, 0);
    let b = ctx.get_field(other, 0);
    Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
}

// equals for Long wrapper
pub(crate) fn native_wrapper_long_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
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
pub(crate) fn native_wrapper_float_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
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
pub(crate) fn native_wrapper_double_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
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

pub(crate) fn native_character_digit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_character_for_digit(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_character_char_count(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_boolean_parse_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_boolean_compare(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

// --- Integer/Long radix helpers ---

pub(crate) fn native_integer_to_string_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_long_parse_long_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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
        Err(_) => Err(rustjvm_types::error::RuntimeError::NumberFormatException {
            message: format!("For input string: \"{text}\""),
        }
        .into()),
    }
}

pub(crate) fn native_long_to_string_radix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
        format!("-{}", i64_to_radix_string(-val, radix))
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
    let mut v = val as u64;
    let mut digits = Vec::new();
    while v > 0 {
        let d = (v % radix as u64) as u32;
        digits.push(char::from_digit(d, radix).unwrap_or('?'));
        v /= radix as u64;
    }
    digits.reverse();
    digits.into_iter().collect()
}

// --- Float ---

pub(crate) fn native_float_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Float");
    ctx.set_field(obj, 0, Value::Float(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_float_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_float_is_nan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_nan() { 1 } else { 0 })))
}

pub(crate) fn native_float_is_infinite(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_infinite() { 1 } else { 0 })))
}

// --- Double (boxing) ---

pub(crate) fn native_double_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Double");
    ctx.set_field(obj, 0, Value::Double(val));
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_wrapper_double_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_double_is_nan(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_nan() { 1 } else { 0 })))
}

pub(crate) fn native_double_is_infinite(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    Ok(Some(Value::Int(if v.is_infinite() { 1 } else { 0 })))
}

// --- Float/Double parsing and utilities (Phase 8 Part 7) ---

fn parse_float_string(s: &str) -> Result<f32, rustjvm_types::error::RuntimeError> {
    let trimmed = s.trim();
    match trimmed {
        "NaN" => Ok(f32::NAN),
        "Infinity" | "+Infinity" => Ok(f32::INFINITY),
        "-Infinity" => Ok(f32::NEG_INFINITY),
        _ => {
            trimmed
                .parse::<f32>()
                .map_err(|_| rustjvm_types::error::RuntimeError::NumberFormatException {
                    message: format!("For input string: \"{s}\""),
                })
        }
    }
}

fn parse_double_string(s: &str) -> Result<f64, rustjvm_types::error::RuntimeError> {
    let trimmed = s.trim();
    match trimmed {
        "NaN" => Ok(f64::NAN),
        "Infinity" | "+Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        _ => {
            trimmed
                .parse::<f64>()
                .map_err(|_| rustjvm_types::error::RuntimeError::NumberFormatException {
                    message: format!("For input string: \"{s}\""),
                })
        }
    }
}

pub(crate) fn native_float_parse_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let val = parse_float_string(&s)?;
    Ok(Some(Value::Float(val)))
}

pub(crate) fn native_double_parse_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
                message: "null".to_string(),
            }
            .into())
        }
    };
    let val = parse_double_string(&s)?;
    Ok(Some(Value::Double(val)))
}

pub(crate) fn native_float_value_of_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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

pub(crate) fn native_double_value_of_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => {
            return Err(rustjvm_types::error::RuntimeError::NumberFormatException {
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

pub(crate) fn native_float_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let s = format_float(v);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_double_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let s = format_double(v);
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

pub(crate) fn native_float_to_int_bits(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_double_to_long_bits(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_float_compare(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_double_compare(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_byte_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let obj = alloc_wrapper(ctx, "java/lang/Byte");
    ctx.set_field(obj, 0, Value::Int(val));
    Ok(Some(Value::Object(Some(obj))))
}

// --- Short ---

pub(crate) fn native_short_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_integer_compare(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_integer_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_long_compare(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(crate) fn native_long_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    use crate::test_utils::mock_ctx;

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
        let r = native_math_max_double(&mut ctx, &[Value::Double(f64::NAN), Value::Double(f64::NAN)]);
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

