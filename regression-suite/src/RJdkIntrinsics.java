/**
 * {@code NativeKind::Intrinsic} means "an accelerated implementation of
 * IDENTICAL semantics". This vector tests the second half of that claim.
 *
 * <h2>Why this file exists</h2>
 *
 * <p>Nothing in the tree checks it. The registrars open with an ambient
 * {@code set_category(NativeKind::Intrinsic)}; {@code Intrinsic} is exempt from
 * shadow retirement and is not the census's {@code native-shadows-bytecode}
 * kind, so {@code cratonvm --jdk-only --jdk-only-report} over a program that
 * calls {@code Math.min} a thousand times emits ZERO {@code java/lang/Math}
 * rows. The project's best instrument is blind to the whole category by
 * construction, and the roadmap's standing judgement — "mostly legitimate
 * acceleration, not roadmap work" — had never been tested when
 * W7-94-math-min-max-nan-and-negative-zero.md found
 * {@code Math.min(1.0, NaN) == 1.0}.
 *
 * <p>Every row below was MEASURED on Microsoft OpenJDK 25.0.3.9 before it was
 * written, and every row was RED on CratonVM at the time of writing (except
 * the blocks explicitly labelled NEGATIVE CONTROL). See
 * docs/known-issues/jdk-only/W7-95-intrinsic-semantics-census.md.
 *
 * <h2>How this file compares</h2>
 *
 * <p>Floating-point results are compared by {@link Double#doubleToRawLongBits}
 * / {@link Float#floatToRawIntBits}, never by {@code ==}. {@code -0.0 == 0.0}
 * is {@code true} and {@code NaN != NaN}, so an equality-shaped check passes
 * against exactly the defects this file hunts. Operands come out of the
 * {@code OPAQUE_*} arrays rather than being written as literals, so neither
 * {@code javac} nor a JIT can constant-fold the call away and answer from the
 * folder instead of from the native.
 *
 * <h2>Mode independence</h2>
 *
 * <p>These are language semantics, not a mode policy: {@code Math.pow}'s
 * special values and {@code Character.isWhitespace}'s membership are the same
 * in {@code --real-jdk} and {@code --jdk-only}. The natives are registered in
 * both arms, so this belongs in {@code CORE_CLASSES}, not
 * {@code JDKONLY_CLASSES}.
 *
 * <h2>{@link #floorDivModOverflowMustNotAbortTheVm()} runs LAST, deliberately</h2>
 *
 * <p>{@code Math.floorDiv(Integer.MIN_VALUE, -1)} reached a bare Rust
 * {@code a / b}. Rust checks division overflow unconditionally, in every
 * profile, so that call PANICS; the panic is not a Java throwable and cannot be
 * caught — it terminates the VM with no further output. Every other block must
 * therefore have reported before this one starts.
 */
public class RJdkIntrinsics {
    static int checks;

    static int mark;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Close a block and assert its own size. The count is a tripwire: a block
     * that silently loses rows to an edit still prints {@code CK}, and a
     * hard-coded number that nobody re-derives is how a shrinking vector goes
     * unnoticed. Mismatch is a failure, not a warning.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RJdkIntrinsics " + name + "=" + n);
    }

    // Operand sources the compiler cannot see through.
    static final double[] OPAQUE_D = {
        1.0, Double.NaN, -1.0, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY,
        0.0, -0.0, Double.MAX_VALUE, Double.MIN_VALUE, 2.0, 10.0, -0.0,
    };
    static final float[] OPAQUE_F = { Float.MAX_VALUE, 0.0f, 1.0f };
    static final int[] OPAQUE_I = { Integer.MIN_VALUE, -1, 0, 1 };
    static final long[] OPAQUE_J = { Long.MIN_VALUE, -1L, 0L, 1L };
    static final char[] OPAQUE_C = {
        '\u00a0', '\u0085', '\u2007', '\u202f', (char) 0x1c, (char) 0x1f, '\t',
        '\u00df', '\ud800', '\udc00', '\u1f88', '\u2160', '\u3007', '\u0660',
        '\u0966', '\uff10', '\uff21', '\u0f20', '\ufb00', 'a', 'A', '0',
    };
    static final int[] OPAQUE_CP = { 0x1d7ce, 0x1f1e6, 0x2764, 0x261d, 0x1f600, 0x0030 };
    static final String[] OPAQUE_S = {
        "  1", "1 ", "1\n", "\u0967\u0968", "nan", "inf", "infinity", "0x1p3",
        "12", "  1.5  ", "NaN", "Infinity",
    };

    // Measured on Microsoft OpenJDK 25.0.3.9.
    static final long NAN_BITS = 0x7ff8000000000000L;
    static final long ULP_OF_MAX_VALUE_BITS = 0x7ca0000000000000L;   // 2^971
    static final int ULP_OF_MAX_FLOAT_BITS = 0x73800000;             // 2^103

    // -----------------------------------------------------------------------
    // Math.pow's special values are a JLS table, not C99's.
    //
    // The JLS: pow(1.0, NaN) is NaN, and pow(x, +-Infinity) is NaN whenever
    // |x| == 1. C99 / IEEE-754 `pow` answers 1.0 for all four, and Rust's
    // `f64::powf` IS that `pow`. StrictMath.pow routes through the ported
    // fdlibm in types/src/fdlibm.rs and gets them right, so this is a
    // twins-that-drift pair inside one file: same class of function, two
    // bodies, only one of them audited.
    // -----------------------------------------------------------------------
    static void powSpecialValues() {
        double one = OPAQUE_D[0];
        double nan = OPAQUE_D[1];
        double negOne = OPAQUE_D[2];
        double pinf = OPAQUE_D[3];
        double ninf = OPAQUE_D[4];
        double zero = OPAQUE_D[5];

        check(Double.doubleToRawLongBits(Math.pow(one, nan)) == NAN_BITS,
                "Math.pow(1.0, NaN) must be NaN (JLS), not 1.0 (C99 pow)");
        check(Double.doubleToRawLongBits(Math.pow(one, pinf)) == NAN_BITS,
                "Math.pow(1.0, +Infinity) must be NaN, |base| == 1");
        check(Double.doubleToRawLongBits(Math.pow(one, ninf)) == NAN_BITS,
                "Math.pow(1.0, -Infinity) must be NaN, |base| == 1");
        check(Double.doubleToRawLongBits(Math.pow(negOne, pinf)) == NAN_BITS,
                "Math.pow(-1.0, +Infinity) must be NaN, |base| == 1");
        check(Double.doubleToRawLongBits(Math.pow(negOne, ninf)) == NAN_BITS,
                "Math.pow(-1.0, -Infinity) must be NaN, |base| == 1");
        check(Double.doubleToRawLongBits(Math.pow(zero, nan)) == NAN_BITS,
                "Math.pow(0.0, NaN) must be NaN");

        // NEGATIVE CONTROL. The exponent-zero rule OUTRANKS the NaN rule, and a
        // fix that returns NaN for every NaN exponent breaks these.
        check(Double.doubleToRawLongBits(Math.pow(nan, zero)) == 0x3ff0000000000000L,
                "Math.pow(NaN, 0.0) must be 1.0 — exponent zero wins over NaN");
        check(Double.doubleToRawLongBits(Math.pow(nan, OPAQUE_D[11])) == 0x3ff0000000000000L,
                "Math.pow(NaN, -0.0) must be 1.0");
        check(Double.doubleToRawLongBits(Math.pow(OPAQUE_D[9], OPAQUE_D[10]))
                        == 0x4090000000000000L,
                "Math.pow(2.0, 10.0) must be exactly 1024.0");
        check(Double.doubleToRawLongBits(Math.pow(OPAQUE_D[11], 3.0)) == Long.MIN_VALUE,
                "Math.pow(-0.0, 3.0) must be -0.0");
        check(Double.doubleToRawLongBits(Math.pow(negOne, 1e100)) == 0x3ff0000000000000L,
                "Math.pow(-1.0, 1e100) must be 1.0 — a FINITE even exponent, not infinity");

        sectionEnd("pow", 11);
    }

    // -----------------------------------------------------------------------
    // Math.ulp at MAX_VALUE. Computing an ulp as `nextUp(x) - x` overflows to
    // +Infinity at the largest finite value; Java's ulp is defined from the
    // EXPONENT and is finite everywhere except NaN and +-Infinity.
    // -----------------------------------------------------------------------
    static void ulpAtTheTop() {
        double max = OPAQUE_D[7];
        float maxF = OPAQUE_F[0];

        check(Double.doubleToRawLongBits(Math.ulp(max)) == ULP_OF_MAX_VALUE_BITS,
                "Math.ulp(Double.MAX_VALUE) must be 2^971, not +Infinity");
        check(Double.doubleToRawLongBits(StrictMath.ulp(max)) == ULP_OF_MAX_VALUE_BITS,
                "StrictMath.ulp(Double.MAX_VALUE) must be 2^971, not +Infinity");
        check(Float.floatToRawIntBits(Math.ulp(maxF)) == ULP_OF_MAX_FLOAT_BITS,
                "Math.ulp(Float.MAX_VALUE) must be 2^103, not +Infinity");
        check(Float.floatToRawIntBits(StrictMath.ulp(maxF)) == ULP_OF_MAX_FLOAT_BITS,
                "StrictMath.ulp(Float.MAX_VALUE) must be 2^103, not +Infinity");

        // NEGATIVE CONTROL. +-Infinity and NaN are the two inputs where an
        // infinite/NaN answer IS the contract, and the rest of the domain was
        // already right.
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_D[3]))
                        == 0x7ff0000000000000L,
                "Math.ulp(+Infinity) must be +Infinity");
        check(Double.isNaN(Math.ulp(OPAQUE_D[1])), "Math.ulp(NaN) must be NaN");
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_D[5]))
                        == Double.doubleToRawLongBits(Double.MIN_VALUE),
                "Math.ulp(0.0) must be Double.MIN_VALUE");
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_D[8]))
                        == Double.doubleToRawLongBits(Double.MIN_VALUE),
                "Math.ulp(Double.MIN_VALUE) must be Double.MIN_VALUE");
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_D[0])) == 0x3cb0000000000000L,
                "Math.ulp(1.0) must be 2^-52");

        sectionEnd("ulp", 9);
    }

    // -----------------------------------------------------------------------
    // java.lang.Character is Java's Unicode table, not Rust's `char` methods.
    // Every classifier below delegated to `char::is_whitespace` /
    // `is_alphabetic` / `is_ascii_digit` / `to_digit` / `to_uppercase`, which
    // implement the UNICODE definitions. Java's are deliberately different, and
    // `char::from_u32` additionally answers `None` for a surrogate, so the
    // (C)C forms returned '\0' for half of the BMP's high plane.
    // -----------------------------------------------------------------------
    static void characterIsJavasTableNotRusts() {
        // The table's own integrity, first. Every assertion below is of the
        // shape "this classifier says NO about this character", which passes
        // vacuously if the character is not the one named — a re-encoding of
        // this file that folded U+00A0 to a plain space would leave the block
        // green and testing nothing. Pin the code points.
        int[] expect = {
            0x00a0, 0x0085, 0x2007, 0x202f, 0x001c, 0x001f, 0x0009,
            0x00df, 0xd800, 0xdc00, 0x1f88, 0x2160, 0x3007, 0x0660,
            0x0966, 0xff10, 0xff21, 0x0f20, 0xfb00, 0x0061, 0x0041, 0x0030,
        };
        check(OPAQUE_C.length == expect.length, "OPAQUE_C changed shape");
        for (int k = 0; k < expect.length; k++) {
            check(OPAQUE_C[k] == expect[k],
                    "OPAQUE_C[" + k + "] must be U+" + Integer.toHexString(expect[k])
                            + " — the source encoding damaged this vector");
        }

        // isWhitespace: Java EXCLUDES every non-breaking space and INCLUDES
        // U+001C..U+001F. Unicode's White_Space property is the other way round
        // on both counts.
        check(!Character.isWhitespace(OPAQUE_C[0]),
                "Character.isWhitespace(U+00A0 NBSP) must be false — non-breaking");
        check(!Character.isWhitespace(OPAQUE_C[1]),
                "Character.isWhitespace(U+0085 NEL) must be false");
        check(!Character.isWhitespace(OPAQUE_C[2]),
                "Character.isWhitespace(U+2007 FIGURE SPACE) must be false — non-breaking");
        check(!Character.isWhitespace(OPAQUE_C[3]),
                "Character.isWhitespace(U+202F NNBSP) must be false — non-breaking");
        check(Character.isWhitespace(OPAQUE_C[4]),
                "Character.isWhitespace(U+001C FILE SEPARATOR) must be true");
        check(Character.isWhitespace(OPAQUE_C[5]),
                "Character.isWhitespace(U+001F UNIT SEPARATOR) must be true");
        check(Character.isWhitespace(OPAQUE_C[6]), "Character.isWhitespace('\\t') must be true");

        // isDigit / digit / getNumericValue over the non-ASCII Nd blocks.
        check(Character.isDigit(OPAQUE_C[13]),
                "Character.isDigit(U+0660 ARABIC-INDIC ZERO) must be true");
        check(Character.isDigit(OPAQUE_C[14]),
                "Character.isDigit(U+0966 DEVANAGARI ZERO) must be true");
        check(Character.isDigit(OPAQUE_C[15]),
                "Character.isDigit(U+FF10 FULLWIDTH ZERO) must be true");
        check(Character.isDigit(OPAQUE_C[17]),
                "Character.isDigit(U+0F20 TIBETAN ZERO) must be true");
        check(Character.isDigit(OPAQUE_CP[0]),
                "Character.isDigit(U+1D7CE MATHEMATICAL BOLD ZERO) must be true");
        check(Character.digit(OPAQUE_C[15], 10) == 0,
                "Character.digit(U+FF10, 10) must be 0");
        check(Character.digit(OPAQUE_C[13], 16) == 0,
                "Character.digit(U+0660, 16) must be 0");
        check(Character.getNumericValue(OPAQUE_C[15]) == 0,
                "Character.getNumericValue(U+FF10) must be 0");
        check(Character.getNumericValue(OPAQUE_C[16]) == 10,
                "Character.getNumericValue(U+FF21 FULLWIDTH A) must be 10");
        check(Character.getNumericValue(OPAQUE_C[11]) == 1,
                "Character.getNumericValue(U+2160 ROMAN NUMERAL ONE) must be 1");

        // isLetter is the FIVE letter categories (Lu Ll Lt Lm Lo). Unicode's
        // Alphabetic property is broader: it takes in Nl and Other_Alphabetic.
        check(!Character.isLetter(OPAQUE_C[11]),
                "Character.isLetter(U+2160) must be false — category Nl, not a letter");
        check(!Character.isLetter(OPAQUE_C[12]),
                "Character.isLetter(U+3007 IDEOGRAPHIC NUMBER ZERO) must be false — Nl");

        // toUpperCase(char)/toLowerCase(char) are the 1:1 mapping. Where the
        // full mapping needs more than one char, Java returns the input
        // UNCHANGED; Rust's `to_uppercase()` yields the first char of the
        // expansion, which is a different character.
        check(Character.toUpperCase(OPAQUE_C[7]) == 0x00df,
                "Character.toUpperCase(U+00DF sharp s) must be U+00DF — 'SS' does not fit");
        check(Character.toUpperCase(OPAQUE_C[18]) == 0xfb00,
                "Character.toUpperCase(U+FB00 ff ligature) must be U+FB00 — 'FF' does not fit");
        check(Character.toUpperCase(OPAQUE_C[10]) == 0x1f88,
                "Character.toUpperCase(U+1F88) must be U+1F88 — titlecase has no 1:1 upper");

        // Surrogates are not scalar values, so `char::from_u32` rejects them.
        // Java maps every unmapped char to itself; it never answers NUL.
        check(Character.toLowerCase(OPAQUE_C[8]) == 0xd800,
                "Character.toLowerCase(U+D800) must be U+D800, not U+0000");
        check(Character.toUpperCase(OPAQUE_C[8]) == 0xd800,
                "Character.toUpperCase(U+D800) must be U+D800, not U+0000");
        check(Character.toLowerCase(OPAQUE_C[9]) == 0xdc00,
                "Character.toLowerCase(U+DC00) must be U+DC00, not U+0000");

        // The emoji predicates (JDK 21+) are their own Unicode properties and
        // are not interchangeable with isEmoji.
        check(!Character.isEmojiPresentation(OPAQUE_CP[2]),
                "Character.isEmojiPresentation(U+2764 HEAVY BLACK HEART) must be false");
        check(!Character.isEmojiPresentation(OPAQUE_CP[3]),
                "Character.isEmojiPresentation(U+261D WHITE UP POINTING INDEX) must be false");
        check(Character.isEmojiPresentation(OPAQUE_CP[1]),
                "Character.isEmojiPresentation(U+1F1E6 REGIONAL INDICATOR A) must be true");
        check(Character.isEmojiComponent(OPAQUE_CP[1]),
                "Character.isEmojiComponent(U+1F1E6) must be true");

        // NEGATIVE CONTROL. ASCII was never wrong and must stay right.
        check(Character.isDigit(OPAQUE_C[21]), "Character.isDigit('0') must be true");
        check(Character.isLetter(OPAQUE_C[19]), "Character.isLetter('a') must be true");
        check(Character.toUpperCase(OPAQUE_C[19]) == 0x0041,
                "Character.toUpperCase('a') must be 'A'");
        check(Character.toLowerCase(OPAQUE_C[20]) == 0x0061,
                "Character.toLowerCase('A') must be 'a'");
        check(Character.digit(OPAQUE_C[21], 10) == 0, "Character.digit('0', 10) must be 0");
        check(Character.getNumericValue(OPAQUE_C[20]) == 10,
                "Character.getNumericValue('A') must be 10");
        check(Character.isEmoji(OPAQUE_CP[4]),
                "Character.isEmoji(U+1F600) must be true");

        sectionEnd("character", 59);
    }

    // -----------------------------------------------------------------------
    // String's code-point family must see UTF-16 code UNITS, including the
    // unpaired ones. Decoding the value through a UTF-8 / scalar-value pipe
    // substitutes U+FFFD, which is a silent data change, and pairing that is
    // never attempted loses the astral code point entirely.
    // -----------------------------------------------------------------------
    static void stringCodePointsAreUtf16() {
        // Built from code units, not written as a literal: a re-encoding of
        // this file must not be able to change what is under test.
        String pair = "a" + new String(Character.toChars(0x1f600)) + "b";
        String loneHigh = "x\ud800y";
        String loneLow = "\udc00a";
        check(pair.length() == 4 && pair.charAt(1) == 0xd83d && pair.charAt(2) == 0xde00,
                "the surrogate-pair fixture must be a + U+D83D U+DE00 + b");

        int[] cps = pair.codePoints().toArray();
        check(cps.length == 3, "\"a<U+1F600>b\".codePoints() must yield 3 code points, not 4");
        check(cps[1] == 0x1f600, "the surrogate PAIR must decode to U+1F600");

        int[] lone = loneHigh.codePoints().toArray();
        check(lone.length == 3, "an unpaired high surrogate is one code point");
        check(lone[1] == 0xd800, "an unpaired high surrogate must stay U+D800, not U+FFFD");
        check(loneLow.codePoints().toArray()[0] == 0xdc00,
                "an unpaired low surrogate must stay U+DC00, not U+FFFD");

        check(loneHigh.codePointAt(1) == 0xd800,
                "String.codePointAt over an unpaired high surrogate must return it, not U+FFFD");
        check("a\ud800".codePointAt(1) == 0xd800,
                "a high surrogate at the END of the string must return itself");
        check(pair.codePointAt(1) == 0x1f600, "String.codePointAt must pair the surrogates");
        check(pair.codePointAt(2) == 0xde00,
                "String.codePointAt on the LOW half must return the low surrogate alone");

        // Bounds. Every one of these is specified to throw.
        boolean threw = false;
        try {
            pair.codePointCount(3, 1);
        } catch (IndexOutOfBoundsException e) {
            threw = true;
        }
        check(threw, "String.codePointCount(begin > end) must throw IndexOutOfBoundsException");

        threw = false;
        try {
            pair.offsetByCodePoints(0, 9);
        } catch (IndexOutOfBoundsException e) {
            threw = true;
        }
        check(threw, "String.offsetByCodePoints past the end must throw IndexOutOfBoundsException");

        threw = false;
        try {
            "ab".repeat(OPAQUE_I[1]);
        } catch (IllegalArgumentException e) {
            threw = true;
        }
        check(threw, "String.repeat(-1) must throw IllegalArgumentException");

        // String.isBlank is Character.isWhitespace, so it inherits the table
        // above; asserted here so a fix that touches only one of the two shows.
        check(!String.valueOf(OPAQUE_C[0]).isBlank(), "NBSP U+00A0 isBlank() must be false");
        check(!String.valueOf(OPAQUE_C[2]).isBlank(), "U+2007 isBlank() must be false");
        check(String.valueOf(OPAQUE_C[4]).isBlank(), "U+001C isBlank() must be true");

        // regionMatches with a NEGATIVE length: the bounds test passes and the
        // comparison loop runs zero times, so the answer is `true`. This is not
        // a typo — it is what the JDK's `while (len-- > 0)` does, and code that
        // passes a computed length depends on it.
        check("ABC".regionMatches(0, "abc", 0, -1),
                "String.regionMatches with len < 0 must be true (vacuous match)");

        // NEGATIVE CONTROL.
        check(!"ABC".regionMatches(0, "abc", -1, 0),
                "String.regionMatches with a negative other-offset must be false");
        check("ABC".regionMatches(true, 0, "abc", 0, 3),
                "String.regionMatches ignore-case must still match");
        check(" \t\n".isBlank(), "\" \\t\\n\".isBlank() must be true");
        check(pair.codePointCount(0, 4) == 3, "codePointCount over the pair must be 3");

        sectionEnd("stringCodePoints", 21);
    }

    // -----------------------------------------------------------------------
    // The parse* family is a JAVA grammar, not Rust's `str::parse`.
    //
    //   * Integer.parseInt does NOT trim: " 1" and "1\n" are errors. (Double
    //     and Float DO trim — the two contracts differ and must not be merged.)
    //   * Integer.parseInt accepts any Character.digit, so Devanagari digits
    //     parse.
    //   * Double.parseDouble accepts "NaN"/"Infinity" EXACTLY, and accepts hex
    //     significands; it rejects "nan"/"inf"/"infinity", which Rust's
    //     `f64::from_str` accepts case-insensitively.
    //   * A null argument is a NullPointerException, not a
    //     NumberFormatException.
    // -----------------------------------------------------------------------
    static void parsersFollowTheJavaGrammar() {
        for (int k = 0; k <= 2; k++) {
            boolean threw = false;
            try {
                Integer.parseInt(OPAQUE_S[k]);
            } catch (NumberFormatException e) {
                threw = true;
            }
            check(threw, "Integer.parseInt(" + escape(OPAQUE_S[k])
                    + ") must throw — parseInt does not trim");
        }
        boolean threw = false;
        try {
            Long.parseLong(OPAQUE_S[1]);
        } catch (NumberFormatException e) {
            threw = true;
        }
        check(threw, "Long.parseLong(\"1 \") must throw — parseLong does not trim");

        threw = false;
        try {
            Short.parseShort(OPAQUE_S[1]);
        } catch (NumberFormatException e) {
            threw = true;
        }
        check(threw, "Short.parseShort(\"1 \") must throw — parseShort does not trim");

        check(Integer.parseInt(OPAQUE_S[3]) == 12,
                "Integer.parseInt(\"<DEVANAGARI 1><DEVANAGARI 2>\") must be 12 — any Nd digit");
        check(Long.parseLong(OPAQUE_S[3]) == 12L,
                "Long.parseLong over Devanagari digits must be 12");

        for (int k = 4; k <= 6; k++) {
            threw = false;
            try {
                Double.parseDouble(OPAQUE_S[k]);
            } catch (NumberFormatException e) {
                threw = true;
            }
            check(threw, "Double.parseDouble(\"" + OPAQUE_S[k]
                    + "\") must throw — Java's tokens are exactly NaN and Infinity");

            threw = false;
            try {
                Float.parseFloat(OPAQUE_S[k]);
            } catch (NumberFormatException e) {
                threw = true;
            }
            check(threw, "Float.parseFloat(\"" + OPAQUE_S[k] + "\") must throw");
        }

        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[7]))
                        == 0x4020000000000000L,
                "Double.parseDouble(\"0x1p3\") must be 8.0 — hex significand is in the grammar");
        check(Float.floatToRawIntBits(Float.parseFloat(OPAQUE_S[7])) == 0x41000000,
                "Float.parseFloat(\"0x1p3\") must be 8.0f");

        threw = false;
        try {
            Double.parseDouble(null);
        } catch (NullPointerException e) {
            threw = true;
        }
        check(threw, "Double.parseDouble(null) must throw NullPointerException, not NFE");

        threw = false;
        try {
            Float.parseFloat(null);
        } catch (NullPointerException e) {
            threw = true;
        }
        check(threw, "Float.parseFloat(null) must throw NullPointerException, not NFE");

        // NEGATIVE CONTROL. Double and Float DO trim, and the exact tokens ARE
        // accepted; a fix that stops trimming everywhere breaks these.
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[9]))
                        == 0x3ff8000000000000L,
                "Double.parseDouble(\"  1.5  \") must be 1.5 — parseDouble DOES trim");
        check(Double.isNaN(Double.parseDouble(OPAQUE_S[10])),
                "Double.parseDouble(\"NaN\") must be NaN");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[11]))
                        == 0x7ff0000000000000L,
                "Double.parseDouble(\"Infinity\") must be +Infinity");
        check(Integer.parseInt(OPAQUE_S[8]) == 12, "Integer.parseInt(\"12\") must be 12");
        check(Integer.parseInt("-2147483648") == Integer.MIN_VALUE,
                "Integer.parseInt(\"-2147483648\") must be Integer.MIN_VALUE");

        sectionEnd("parsers", 22);
    }

    static String escape(String s) {
        StringBuilder sb = new StringBuilder("\"");
        for (int k = 0; k < s.length(); k++) {
            char c = s.charAt(k);
            if (c < 0x20 || c > 0x7e) {
                sb.append(String.format("\\u%04x", (int) c));
            } else {
                sb.append(c);
            }
        }
        return sb.append('"').toString();
    }

    // -----------------------------------------------------------------------
    // MUST RUN LAST — see the class comment. On a VM whose floorDiv/floorMod
    // natives divide without guarding MIN/-1, the first line here kills the
    // process; nothing after it, in this method or in main, will report.
    //
    // Java's contract: the quotient MIN/-1 overflows and WRAPS to MIN, exactly
    // as the `idiv` bytecode does (JVMS 6.5 idiv), and floorMod is 0.
    // -----------------------------------------------------------------------
    static void floorDivModOverflowMustNotAbortTheVm() {
        int minI = OPAQUE_I[0];
        int negOneI = OPAQUE_I[1];
        long minJ = OPAQUE_J[0];
        long negOneJ = OPAQUE_J[1];

        check(Math.floorDiv(minI, negOneI) == Integer.MIN_VALUE,
                "Math.floorDiv(Integer.MIN_VALUE, -1) must wrap to Integer.MIN_VALUE");
        check(Math.floorDiv(minJ, negOneJ) == Long.MIN_VALUE,
                "Math.floorDiv(Long.MIN_VALUE, -1L) must wrap to Long.MIN_VALUE");
        check(Math.floorMod(minI, negOneI) == 0,
                "Math.floorMod(Integer.MIN_VALUE, -1) must be 0");
        check(Math.floorMod(minJ, negOneJ) == 0L,
                "Math.floorMod(Long.MIN_VALUE, -1L) must be 0");
        check(StrictMath.floorDiv(minI, negOneI) == Integer.MIN_VALUE,
                "StrictMath.floorDiv(Integer.MIN_VALUE, -1) must wrap to Integer.MIN_VALUE");
        check(StrictMath.floorMod(minI, negOneI) == 0,
                "StrictMath.floorMod(Integer.MIN_VALUE, -1) must be 0");
        check(minI / negOneI == Integer.MIN_VALUE,
                "the idiv OPCODE must wrap too — this is the same rule, one dispatch away");

        // NEGATIVE CONTROL. Division by zero is still an ArithmeticException,
        // and the ordinary floor-rounding is unchanged.
        boolean threw = false;
        try {
            Math.floorDiv(OPAQUE_I[3], OPAQUE_I[2]);
        } catch (ArithmeticException e) {
            threw = true;
        }
        check(threw, "Math.floorDiv(1, 0) must throw ArithmeticException");
        check(Math.floorDiv(-7, 2) == -4, "Math.floorDiv(-7, 2) must be -4");
        check(Math.floorMod(-7, 2) == 1, "Math.floorMod(-7, 2) must be 1");

        sectionEnd("floorDivMod", 10);
    }

    public static void main(String[] args) {
        powSpecialValues();
        ulpAtTheTop();
        characterIsJavasTableNotRusts();
        stringCodePointsAreUtf16();
        parsersFollowTheJavaGrammar();
        floorDivModOverflowMustNotAbortTheVm();
        System.out.println("CK RJdkIntrinsics checks=" + checks);
        System.out.println("PASS RJdkIntrinsics (" + checks + " checks)");
    }
}
