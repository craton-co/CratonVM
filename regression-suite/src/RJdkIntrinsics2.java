import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.CharBuffer;
import java.util.Arrays;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Locale;
import java.util.Random;
import java.util.UUID;
import java.util.concurrent.atomic.AtomicReferenceArray;

/**
 * Second-generation {@code NativeKind::Intrinsic} census vector: the 40% of the
 * category that the FIRST census never called.
 *
 * <h2>The coverage arithmetic this file exists to close</h2>
 *
 * <p>W7-95 measured {@code Intrinsic} and found 39 divergent triples, six of
 * them VM-fatal. Its own numbers say the sample was partial: <b>645</b> rows are
 * registered {@code Intrinsic} (614 distinct triples — 31 rows are duplicate
 * registrations), and only <b>258</b> of them were invoked by the probe. The
 * other ~387 have never had their semantics checked by anything, and the
 * untested remainder is not random — it is whole families
 * ({@code HexFormat}, {@code Base64}, {@code UUID}, {@code java.util.Random},
 * {@code String.format}, the {@code StrictMath} integral forms, the
 * {@code AtomicReferenceArray} and {@code CharBuffer} indexed accessors).
 *
 * <p>See docs/known-issues/jdk-only/W8-C3-1-intrinsic-census-round-2.md for the
 * arithmetic, the triple-by-triple target list, and what is still unreachable.
 *
 * <h2>The risk model — why these triples and not others</h2>
 *
 * <p>W7-95's own conclusion is the prior: the failures cluster on three seams,
 * and "any intrinsic that reaches for a Rust standard-library method with a
 * plausibly-matching name is a suspect". Every family below is chosen for one
 * of five concrete Rust-vs-Java hazards, in descending order of severity:
 *
 * <ol>
 *   <li><b>Integer division and overflow.</b> Rust checks division overflow
 *       UNCONDITIONALLY — release as well as debug — so {@code MIN_VALUE / -1}
 *       panics, and a Rust panic is not a Java throwable: it terminates the VM.
 *       Java's rule is the {@code idiv} opcode's (JVMS 6.5): the quotient
 *       overflows and WRAPS. See {@link #divmod()} and {@link #strictExact()}.
 *   <li><b>Array indexing and slicing.</b> Rust panics on an out-of-bounds
 *       index; Java throws a catchable exception whose exact class is
 *       specified. See {@link #bounds()}.
 *   <li><b>Anything taking a {@code String}.</b> A Rust {@code str} cannot hold
 *       an unpaired UTF-16 surrogate, and Rust's parsers implement Rust's
 *       grammars. See {@link #boolparse()}, {@link #floatfmt()},
 *       {@link #uuid()}, {@link #b64()}.
 *   <li><b>Float and double special values.</b> NaN, +-0.0, +-Infinity,
 *       MAX_VALUE, MIN_VALUE, subnormals. See {@link #floatfmt()}.
 *   <li><b>A Rust standard-library equivalent with different edge semantics.</b>
 *       {@code char::is_uppercase} is Unicode's {@code Uppercase} property,
 *       {@code str::parse::<bool>} accepts only lowercase, {@code f32}'s
 *       {@code Display} is not {@code Float.toString}'s shortest-round-trip
 *       algorithm, and a seeded {@code java.util.Random} is a SPECIFIED LCG
 *       whose every output is fixed by javadoc. See {@link #charcls()},
 *       {@link #random()}, {@link #hex()}, {@link #strfmt()}.
 * </ol>
 *
 * <h2>How this file compares, and how it is driven</h2>
 *
 * <p>Floating-point results are compared by {@link Double#doubleToRawLongBits} /
 * {@link Float#floatToRawIntBits}, never by {@code ==}: {@code -0.0 == 0.0} is
 * {@code true} and {@code NaN != NaN}, so an equality-shaped check passes
 * against exactly the defects this file hunts. Operands come out of the
 * {@code OPAQUE_*} arrays rather than being written as literals, so neither
 * {@code javac} nor a JIT that reimplements the native as a thin direct helper
 * can answer from the folder instead of from the native.
 *
 * <p>Every expected value in this file was MEASURED on Microsoft OpenJDK
 * 25.0.3+9 before it was written. None of them is remembered.
 *
 * <p><b>{@code --only=<family>} exists because a Rust panic truncates the
 * run.</b> With no arguments — which is how {@code run.sh} drives it — the
 * eleven families run in ascending order of how likely each is to ABORT the VM
 * rather than fail an assertion, so a VM that dies in {@link #divmod()} has
 * already reported the other ten. Passing {@code --only=divmod} runs that
 * family alone, which is the only way to learn anything about a family whose
 * predecessor kills the process. {@code --list} prints the family names. The
 * two families most likely to abort additionally print a
 * {@code CK RJdkIntrinsics2 <family>-step=<name>} line BEFORE each risky call,
 * so the last line on stdout names the call that killed the VM.
 *
 * <h2>Mode independence</h2>
 *
 * <p>These are language semantics, not a mode policy, and the natives are
 * registered in both arms — so this belongs in {@code CORE_CLASSES}, exactly
 * like {@code RJdkIntrinsics}.
 */
public class RJdkIntrinsics2 {
    static int checks;

    static int mark;

    /** Sink for a value whose only purpose is to keep a call from being elided. */
    static int sink;

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
        System.out.println("CK RJdkIntrinsics2 " + name + "=" + n);
    }

    /**
     * A progress marker printed BEFORE a call that may abort the VM instead of
     * throwing. On a VM that panics, the last of these on stdout names the
     * call that killed it; on a correct VM the sequence is deterministic, so it
     * diffs clean against the oracle. Used only by the two families whose
     * hazard is a Rust panic rather than a wrong answer.
     */
    static void step(String family, String what) {
        System.out.println("CK RJdkIntrinsics2 " + family + "-step=" + what);
    }

    /** The class of the throwable {@code op} produced, or {@code "none"}. */
    static String nameOf(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    // Operand sources neither javac nor a JIT can see through.
    static final int[] OPAQUE_I = {
        Integer.MIN_VALUE, Integer.MAX_VALUE, -1, 0, 1, 2, 3, 255, -255, -7, 36, 16, 10, 9,
    };
    static final long[] OPAQUE_J = {
        Long.MIN_VALUE, Long.MAX_VALUE, -1L, 0L, 1L, 2L, -7L, -255L, 42L, -2L,
    };
    static final double[] OPAQUE_D = { -0.0, 0.0, Double.NaN, 1.5, Math.PI, 1234.5, 0.5, 2.5 };
    static final float[] OPAQUE_F = { -0.0f, 0.0f, Float.NaN, Float.MAX_VALUE, Float.MIN_VALUE };
    static final int[] OPAQUE_CP = {
        0x00aa, 0x01c5, 0x2160, 0x2170, 0x00b2, 0x001c, 0x007f, 0x009f, 0x00a0,
        0x1f600, 0xffff, 0x10000, 0x10ffff, 0x110000, 0xdbff, 0xdc00, 0xd800, 0xdfff,
        0x10428, 0x10400, 0x00df, 0x1f3fb, 0x1f44d, 0x2764, 0x1d7ce, 0x00bc, 0x0f20,
    };
    static final String[] OPAQUE_S = {
        "TRUE", "True", "tRuE", "1", "yes", "", " true", "z", "Z", "-z", "0", "+1",
        "--1", "-", "2147483648", "-2147483648", "१२", "ｆｆ",
        "9223372036854775808", "-9223372036854775808", "128", "-128", "7f", "80",
        "32768", "-32768", "ffff", "7fff", "nan", "inf", "0x1p3", "1.0f", "1.0d",
        "1e-46", "1e40", "1e-324", "1e400", "-0.0", "1.0dd", "  1.5  ",
    };

    // -----------------------------------------------------------------------
    // 1. charcls — the java.lang.Character predicates W7-95 did NOT call.
    //
    // W7-95 pinned isWhitespace/isDigit/isLetter/digit/getNumericValue/case
    // mapping. Sixteen registered Character triples were never invoked at all:
    // isLowerCase / isUpperCase (both arities), isISOControl, forDigit,
    // charCount, isValidCodePoint, isBmpCodePoint, isHighSurrogate,
    // isLowSurrogate, isEmojiModifier, isEmojiModifierBase, and the (I)I case
    // mappings over ASTRAL code points. Same seam, same predicted mechanism:
    // Rust's `char` methods implement UNICODE's definitions, and Java's are
    // deliberately different.
    //
    // The load-bearing pair is U+2160 ROMAN NUMERAL ONE: Java says
    // isUpperCase=true (Other_Uppercase) and isLetter=false (category Nl). A
    // body that answers either predicate from `char::is_alphabetic` cannot get
    // both rows right.
    // -----------------------------------------------------------------------
    static void charcls() {
        // Fixture integrity first. Every row below is of the shape "this
        // classifier says X about this code point", which passes vacuously if
        // the code point is not the one named.
        int[] expect = {
            0x00aa, 0x01c5, 0x2160, 0x2170, 0x00b2, 0x001c, 0x007f, 0x009f, 0x00a0,
            0x1f600, 0xffff, 0x10000, 0x10ffff, 0x110000, 0xdbff, 0xdc00, 0xd800, 0xdfff,
            0x10428, 0x10400, 0x00df, 0x1f3fb, 0x1f44d, 0x2764, 0x1d7ce, 0x00bc, 0x0f20,
        };
        check(OPAQUE_CP.length == expect.length, "OPAQUE_CP changed shape");
        for (int k = 0; k < expect.length; k++) {
            check(OPAQUE_CP[k] == expect[k], "OPAQUE_CP[" + k + "] must be U+"
                    + Integer.toHexString(expect[k]) + " — the fixture was damaged");
        }

        // isLowerCase / isUpperCase are Java's Other_Lowercase / Other_Uppercase
        // -extended letter tests, and they are NOT each other's complement.
        check(Character.isLowerCase((char) OPAQUE_CP[0]),
                "Character.isLowerCase(U+00AA FEMININE ORDINAL) must be true");
        check(!Character.isLowerCase((char) OPAQUE_CP[1]),
                "Character.isLowerCase(U+01C5 titlecase Dz) must be false — it is Lt");
        check(!Character.isUpperCase((char) OPAQUE_CP[1]),
                "Character.isUpperCase(U+01C5) must be false — Lt is neither");
        check(Character.isUpperCase((char) OPAQUE_CP[2]),
                "Character.isUpperCase(U+2160 ROMAN NUMERAL ONE) must be true — Other_Uppercase");
        check(!Character.isLetter((char) OPAQUE_CP[2]),
                "Character.isLetter(U+2160) must be false — the SAME code point, category Nl");
        check(Character.isLowerCase((char) OPAQUE_CP[3]),
                "Character.isLowerCase(U+2170 SMALL ROMAN NUMERAL ONE) must be true");
        check(!Character.isUpperCase((char) OPAQUE_CP[4]),
                "Character.isUpperCase(U+00B2 SUPERSCRIPT TWO) must be false");
        check(Character.isLowerCase(OPAQUE_CP[18]),
                "Character.isLowerCase(U+10428 DESERET SMALL LONG I) must be true — astral");
        check(Character.isUpperCase(OPAQUE_CP[19]),
                "Character.isUpperCase(U+10400 DESERET CAPITAL LONG I) must be true — astral");
        check(Character.isLetter(OPAQUE_CP[19]),
                "Character.isLetter(U+10400) must be true — astral Lu");

        // isISOControl is a pure range test (0x00-0x1F, 0x7F-0x9F) and is
        // defined for every int, including negatives.
        check(Character.isISOControl(OPAQUE_CP[5]), "Character.isISOControl(0x1C) must be true");
        check(Character.isISOControl(OPAQUE_CP[6]), "Character.isISOControl(0x7F DEL) must be true");
        check(Character.isISOControl(OPAQUE_CP[7]), "Character.isISOControl(0x9F) must be true");
        check(!Character.isISOControl(OPAQUE_CP[8]),
                "Character.isISOControl(0xA0 NBSP) must be false — one past the C1 range");
        check(!Character.isISOControl(OPAQUE_I[2]),
                "Character.isISOControl(-1) must be false, not a panic");

        // forDigit is specified to return the NUL character for every input it
        // cannot map — never to throw and never to index out of a table.
        check(Character.forDigit(35, OPAQUE_I[10]) == 'z', "Character.forDigit(35, 36) must be 'z'");
        check(Character.forDigit(15, OPAQUE_I[11]) == 'f', "Character.forDigit(15, 16) must be 'f'");
        check(Character.forDigit(OPAQUE_I[12], OPAQUE_I[12]) == 0,
                "Character.forDigit(10, 10) must be U+0000 — digit >= radix");
        check(Character.forDigit(OPAQUE_I[2], OPAQUE_I[11]) == 0,
                "Character.forDigit(-1, 16) must be U+0000, not a panic");
        check(Character.forDigit(OPAQUE_I[3], OPAQUE_I[4]) == 0,
                "Character.forDigit(0, 1) must be U+0000 — radix below MIN_RADIX");
        check(Character.forDigit(OPAQUE_I[3], 37) == 0,
                "Character.forDigit(0, 37) must be U+0000 — radix above MAX_RADIX");

        // charCount / isValidCodePoint / isBmpCodePoint are arithmetic on an
        // int that is NOT required to be a valid code point.
        check(Character.charCount(OPAQUE_CP[9]) == 2, "Character.charCount(U+1F600) must be 2");
        check(Character.charCount(OPAQUE_CP[10]) == 1, "Character.charCount(U+FFFF) must be 1");
        check(Character.charCount(OPAQUE_I[2]) == 1,
                "Character.charCount(-1) must be 1 — below MIN_SUPPLEMENTARY, not a panic");
        check(!Character.isValidCodePoint(OPAQUE_CP[13]),
                "Character.isValidCodePoint(0x110000) must be false");
        check(!Character.isValidCodePoint(OPAQUE_I[2]),
                "Character.isValidCodePoint(-1) must be false");
        check(Character.isValidCodePoint(OPAQUE_CP[12]),
                "Character.isValidCodePoint(U+10FFFF) must be true");
        check(Character.isBmpCodePoint(OPAQUE_CP[10]),
                "Character.isBmpCodePoint(U+FFFF) must be true");
        check(!Character.isBmpCodePoint(OPAQUE_CP[11]),
                "Character.isBmpCodePoint(0x10000) must be false");
        check(!Character.isBmpCodePoint(OPAQUE_I[2]),
                "Character.isBmpCodePoint(-1) must be false");

        // The surrogate range tests. A `char::from_u32`-based body answers None
        // for the whole block and cannot distinguish its two halves.
        check(Character.isHighSurrogate((char) OPAQUE_CP[14]),
                "Character.isHighSurrogate(U+DBFF) must be true — top of the high half");
        check(!Character.isHighSurrogate((char) OPAQUE_CP[15]),
                "Character.isHighSurrogate(U+DC00) must be false — that is the LOW half");
        check(Character.isLowSurrogate((char) OPAQUE_CP[17]),
                "Character.isLowSurrogate(U+DFFF) must be true");
        check(!Character.isLowSurrogate((char) OPAQUE_CP[16]),
                "Character.isLowSurrogate(U+D800) must be false");

        // The (I)I case mappings over ASTRAL code points and over inputs that
        // are not scalar values. Java maps every unmapped int to ITSELF.
        check(Character.toUpperCase(OPAQUE_CP[18]) == 0x10400,
                "Character.toUpperCase(U+10428) must be U+10400 — astral case mapping");
        check(Character.toLowerCase(OPAQUE_CP[19]) == 0x10428,
                "Character.toLowerCase(U+10400) must be U+10428");
        check(Character.toUpperCase(OPAQUE_CP[16]) == 0xd800,
                "Character.toUpperCase(int U+D800) must be U+D800, not U+0000");
        check(Character.toLowerCase(OPAQUE_CP[16]) == 0xd800,
                "Character.toLowerCase(int U+D800) must be U+D800, not U+0000");
        check(Character.toUpperCase(OPAQUE_CP[9]) == 0x1f600,
                "Character.toUpperCase(U+1F600) must be itself — no mapping");
        check(Character.toUpperCase(OPAQUE_CP[20]) == 0x00df,
                "Character.toUpperCase(int U+00DF sharp s) must be U+00DF — 'SS' does not fit");
        check(Character.toUpperCase(OPAQUE_I[2]) == -1,
                "Character.toUpperCase(-1) must be -1 — every unmapped int maps to itself");

        // digit(C,I) at the radix boundaries, and digit over an astral Nd.
        check(Character.digit('z', OPAQUE_I[10]) == 35, "Character.digit('z', 36) must be 35");
        check(Character.digit('a', OPAQUE_I[12]) == -1,
                "Character.digit('a', 10) must be -1 — not a digit in this radix");
        check(Character.digit('0', OPAQUE_I[4]) == -1, "Character.digit('0', 1) must be -1");
        check(Character.digit('0', 37) == -1, "Character.digit('0', 37) must be -1");
        check(Character.digit(OPAQUE_CP[24], OPAQUE_I[12]) == 0,
                "Character.digit(U+1D7CE MATH BOLD ZERO, 10) must be 0");
        check(Character.digit((char) OPAQUE_CP[26], OPAQUE_I[12]) == 0,
                "Character.digit(U+0F20 TIBETAN ZERO, 10) must be 0");
        check(Character.getNumericValue((char) OPAQUE_CP[25]) == -2,
                "Character.getNumericValue(U+00BC ONE QUARTER) must be -2, not -1");

        // The emoji properties W7-95 sampled at four code points. Two of the
        // five properties already disagreed there, so the remaining two are
        // tested here rather than assumed.
        check(Character.isEmojiModifier(OPAQUE_CP[21]),
                "Character.isEmojiModifier(U+1F3FB SKIN TONE 1-2) must be true");
        check(!Character.isEmojiModifier(OPAQUE_CP[23]),
                "Character.isEmojiModifier(U+2764) must be false");
        check(Character.isEmojiModifierBase(OPAQUE_CP[22]),
                "Character.isEmojiModifierBase(U+1F44D THUMBS UP) must be true");
        check(!Character.isEmojiModifierBase(OPAQUE_CP[23]),
                "Character.isEmojiModifierBase(U+2764) must be false");

        // NEGATIVE CONTROL. ASCII and the plain BMP were never wrong.
        check(Character.isLowerCase('a'), "Character.isLowerCase('a') must be true");
        check(Character.isUpperCase('A'), "Character.isUpperCase('A') must be true");
        check(!Character.isUpperCase('a'), "Character.isUpperCase('a') must be false");
        check(!Character.isISOControl(' '), "Character.isISOControl(' ') must be false");
        check(Character.isLetterOrDigit(OPAQUE_CP[24]),
                "Character.isLetterOrDigit(U+1D7CE) must be true");
        check("a".equals(Character.toString('a')), "Character.toString('a') must be \"a\"");

        sectionEnd("charcls", 86);
    }

    // -----------------------------------------------------------------------
    // 2. boolparse — the radix parsers, the radix formatters, and
    //    Boolean.parseBoolean.
    //
    // W7-95 measured the no-radix parseInt/parseLong/parseShort. The (String,I)
    // overloads are SEPARATE registered triples and were never invoked, and so
    // were Byte.parseByte, every toString(x, radix), toHexString, getInteger,
    // getLong and the whole Boolean family.
    //
    // The predicted mechanism is W7-95's: `text.trim().parse::<i32>()` is Rust's
    // grammar. Two specific Rust reflexes are pinned here:
    //   * `str::parse::<bool>()` accepts ONLY "true"/"false" lowercase.
    //     Boolean.parseBoolean is case-INSENSITIVE, so "TRUE" is the row that
    //     separates the two.
    //   * `i32::from_str_radix` accepts only ASCII alphanumerics, where Java
    //     accepts any Character.digit — Devanagari and fullwidth digits parse.
    // -----------------------------------------------------------------------
    static void boolparse() {
        check(Boolean.parseBoolean(OPAQUE_S[0]),
                "Boolean.parseBoolean(\"TRUE\") must be true — the comparison is case-INSENSITIVE");
        check(Boolean.parseBoolean(OPAQUE_S[1]), "Boolean.parseBoolean(\"True\") must be true");
        check(Boolean.parseBoolean(OPAQUE_S[2]), "Boolean.parseBoolean(\"tRuE\") must be true");
        check(!Boolean.parseBoolean(OPAQUE_S[3]), "Boolean.parseBoolean(\"1\") must be false");
        check(!Boolean.parseBoolean(OPAQUE_S[4]), "Boolean.parseBoolean(\"yes\") must be false");
        check(!Boolean.parseBoolean(OPAQUE_S[5]), "Boolean.parseBoolean(\"\") must be false");
        check(!Boolean.parseBoolean(null),
                "Boolean.parseBoolean(null) must be FALSE — it does not throw");
        check(!Boolean.parseBoolean(OPAQUE_S[6]),
                "Boolean.parseBoolean(\" true\") must be false — parseBoolean does not trim");
        check(Boolean.valueOf(OPAQUE_S[0]).booleanValue(), "Boolean.valueOf(\"TRUE\") must be true");
        check(Boolean.hashCode(true) == 1231, "Boolean.hashCode(true) must be exactly 1231");
        check(Boolean.hashCode(false) == 1237, "Boolean.hashCode(false) must be exactly 1237");
        check(Boolean.TRUE.hashCode() == 1231, "Boolean.TRUE.hashCode() must be 1231");
        check(Boolean.compare(true, false) == 1, "Boolean.compare(true, false) must be 1");
        check(Boolean.compare(false, true) == -1, "Boolean.compare(false, true) must be -1");
        check(Boolean.compare(true, true) == 0, "Boolean.compare(true, true) must be 0");
        check("true".equals(Boolean.toString(true)), "Boolean.toString(true) must be \"true\"");
        check(!Boolean.getBoolean("cratonvm.no.such.prop"),
                "Boolean.getBoolean(absent property) must be false");
        check(Integer.getInteger("cratonvm.no.such.prop", OPAQUE_I[6]).intValue() == 3,
                "Integer.getInteger(absent, 3) must return the default");
        check(Long.getLong("cratonvm.no.such.prop", OPAQUE_J[4]).longValue() == 1L,
                "Long.getLong(absent, 1) must return the default");

        // parseInt/parseLong with an explicit radix.
        check(Integer.parseInt(OPAQUE_S[7], OPAQUE_I[10]) == 35,
                "Integer.parseInt(\"z\", 36) must be 35");
        check(Integer.parseInt(OPAQUE_S[8], OPAQUE_I[10]) == 35,
                "Integer.parseInt(\"Z\", 36) must be 35 — case-insensitive digits");
        check(Integer.parseInt(OPAQUE_S[9], OPAQUE_I[10]) == -35,
                "Integer.parseInt(\"-z\", 36) must be -35");
        check(Long.parseLong(OPAQUE_S[9], OPAQUE_I[10]) == -35L,
                "Long.parseLong(\"-z\", 36) must be -35");
        check(Integer.parseInt(OPAQUE_S[11]) == 1,
                "Integer.parseInt(\"+1\") must be 1 — a leading plus IS in the grammar");
        check(Integer.parseInt(OPAQUE_S[15], OPAQUE_I[12]) == Integer.MIN_VALUE,
                "Integer.parseInt(\"-2147483648\", 10) must be Integer.MIN_VALUE");
        check(Long.parseLong(OPAQUE_S[19]) == Long.MIN_VALUE,
                "Long.parseLong(\"-9223372036854775808\") must be Long.MIN_VALUE");
        check(Integer.parseInt(OPAQUE_S[16], OPAQUE_I[12]) == 12,
                "Integer.parseInt(<DEVANAGARI 1 2>, 10) must be 12 — any Character.digit");
        check(Integer.parseInt(OPAQUE_S[17], OPAQUE_I[11]) == 255,
                "Integer.parseInt(<FULLWIDTH f f>, 16) must be 255 — non-ASCII hex digits");
        check(Byte.parseByte(OPAQUE_S[16]) == 12,
                "Byte.parseByte(<DEVANAGARI 1 2>) must be 12");
        check(Short.parseShort(OPAQUE_S[16]) == 12,
                "Short.parseShort(<DEVANAGARI 1 2>) must be 12");
        check(Byte.parseByte(OPAQUE_S[21]) == -128, "Byte.parseByte(\"-128\") must be -128");
        check(Byte.parseByte(OPAQUE_S[22], OPAQUE_I[11]) == 127,
                "Byte.parseByte(\"7f\", 16) must be 127");
        check(Short.parseShort(OPAQUE_S[25]) == -32768,
                "Short.parseShort(\"-32768\") must be -32768");
        check(Short.parseShort(OPAQUE_S[27], OPAQUE_I[11]) == 32767,
                "Short.parseShort(\"7fff\", 16) must be 32767");

        // Every rejection the grammar requires. The radix bounds are the
        // interesting half: Rust's from_str_radix PANICS on a radix outside
        // 2..=36, where Java throws a catchable NumberFormatException.
        String[] badInt = { OPAQUE_S[12], OPAQUE_S[5], OPAQUE_S[13], OPAQUE_S[14] };
        String[] badIntWhy = { "\"--1\"", "\"\"", "\"-\"", "\"2147483648\" (overflow)" };
        for (int k = 0; k < badInt.length; k++) {
            Throwable t = null;
            try {
                Integer.parseInt(badInt[k]);
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.NumberFormatException".equals(nameOf(t)),
                    "Integer.parseInt(" + badIntWhy[k] + ") must throw NumberFormatException, got "
                            + nameOf(t));
        }
        Throwable t = null;
        try {
            Integer.parseInt(OPAQUE_S[10], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Integer.parseInt(\"0\", 1) must throw NumberFormatException — radix < MIN_RADIX,"
                        + " which is a PANIC in Rust's from_str_radix; got " + nameOf(t));
        t = null;
        try {
            Integer.parseInt(OPAQUE_S[10], 37);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Integer.parseInt(\"0\", 37) must throw NumberFormatException — radix >"
                        + " MAX_RADIX; got " + nameOf(t));
        t = null;
        try {
            Long.parseLong(OPAQUE_S[18]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Long.parseLong(\"9223372036854775808\") must throw — one past Long.MAX_VALUE");
        t = null;
        try {
            Byte.parseByte(OPAQUE_S[20]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Byte.parseByte(\"128\") must throw — Byte's range is checked AFTER the parse");
        t = null;
        try {
            Byte.parseByte(OPAQUE_S[23], OPAQUE_I[11]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Byte.parseByte(\"80\", 16) must throw — 128 does not fit a signed byte");
        t = null;
        try {
            Short.parseShort(OPAQUE_S[24]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Short.parseShort(\"32768\") must throw");
        t = null;
        try {
            Short.parseShort(OPAQUE_S[26], OPAQUE_I[11]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Short.parseShort(\"ffff\", 16) must throw — 65535 does not fit a signed short");

        // toString(x, radix) is the inverse grammar, and an INVALID radix is
        // specified to fall back to 10 rather than to throw.
        check("-zik0zk".equals(Integer.toString(Integer.MIN_VALUE, OPAQUE_I[10])),
                "Integer.toString(MIN_VALUE, 36) must be \"-zik0zk\"");
        check("ff".equals(Integer.toString(OPAQUE_I[7], OPAQUE_I[11])),
                "Integer.toString(255, 16) must be \"ff\"");
        check("-ff".equals(Integer.toString(OPAQUE_I[8], OPAQUE_I[11])),
                "Integer.toString(-255, 16) must be \"-ff\"");
        check("255".equals(Integer.toString(OPAQUE_I[7], OPAQUE_I[4])),
                "Integer.toString(255, 1) must be \"255\" — an invalid radix falls back to 10");
        check("255".equals(Integer.toString(OPAQUE_I[7], 37)),
                "Integer.toString(255, 37) must be \"255\" — invalid radix, not a panic");
        check("-1y2p0ij32e8e8".equals(Long.toString(Long.MIN_VALUE, OPAQUE_I[10])),
                "Long.toString(MIN_VALUE, 36) must be \"-1y2p0ij32e8e8\"");
        check("-ff".equals(Long.toString(OPAQUE_J[7], OPAQUE_I[11])),
                "Long.toString(-255, 16) must be \"-ff\"");
        check("ffffffff".equals(Integer.toHexString(OPAQUE_I[2])),
                "Integer.toHexString(-1) must be \"ffffffff\" — UNSIGNED, not \"-1\"");
        check("0".equals(Integer.toHexString(OPAQUE_I[3])), "Integer.toHexString(0) must be \"0\"");
        check("80000000".equals(Integer.toHexString(Integer.MIN_VALUE)),
                "Integer.toHexString(MIN_VALUE) must be \"80000000\"");
        check("-2147483648".equals(Integer.toString(Integer.MIN_VALUE)),
                "Integer.toString(MIN_VALUE) must be \"-2147483648\"");
        check("-9223372036854775808".equals(Long.toString(Long.MIN_VALUE)),
                "Long.toString(MIN_VALUE) must be \"-9223372036854775808\"");

        sectionEnd("boolparse", 57);
    }

    // -----------------------------------------------------------------------
    // 3. floatfmt — Float's formatting and the NaN-CANONICALISING bit accessors.
    //
    // W7-95 measured Double.toString across its formatting breakpoints and
    // Double.compare/hashCode over NaN and -0.0. Float's twins — a separate
    // registered triple each — were never invoked, and neither was
    // Float.valueOf(String) / Double.valueOf(String), which are DIFFERENT
    // triples from parseFloat/parseDouble and carry a different null contract.
    //
    // The sharpest row here is Float.floatToIntBits, which is specified to
    // COLLAPSE every NaN bit pattern to 0x7fc00000, where floatToRawIntBits
    // preserves it. A Rust `f32::to_bits()` is the RAW one. Two registered
    // triples, one Rust method, and only one of them is correct.
    // -----------------------------------------------------------------------
    static void floatfmt() {
        check("0.1".equals(Float.toString(0.1f)), "Float.toString(0.1f) must be \"0.1\"");
        check("3.4028235E38".equals(Float.toString(OPAQUE_F[3])),
                "Float.toString(Float.MAX_VALUE) must be \"3.4028235E38\"");
        check("1.4E-45".equals(Float.toString(OPAQUE_F[4])),
                "Float.toString(Float.MIN_VALUE) must be \"1.4E-45\" — the subnormal");
        check("1.1754944E-38".equals(Float.toString(Float.MIN_NORMAL)),
                "Float.toString(Float.MIN_NORMAL) must be \"1.1754944E-38\"");
        check("-0.0".equals(Float.toString(OPAQUE_F[0])),
                "Float.toString(-0.0f) must be \"-0.0\", sign preserved");
        check("1.0E7".equals(Float.toString(1e7f)),
                "Float.toString(1e7f) must be \"1.0E7\" — the exponent breakpoint");
        check("0.001".equals(Float.toString(1e-3f)),
                "Float.toString(1e-3f) must be \"0.001\" — below the breakpoint");
        check("1.0E20".equals(Float.toString(1e20f)), "Float.toString(1e20f) must be \"1.0E20\"");
        check("NaN".equals(Float.toString(OPAQUE_F[2])), "Float.toString(NaN) must be \"NaN\"");
        check("Infinity".equals(Float.toString(Float.POSITIVE_INFINITY)),
                "Float.toString(+Infinity) must be \"Infinity\"");
        check("1.0".equals(Float.toString(1.0f)),
                "Float.toString(1.0f) must be \"1.0\" — Rust's f32 Display prints \"1\"");
        check("1.1".equals(Float.toString(1.1f)),
                "Float.toString(1.1f) must be \"1.1\" — shortest round-trip, not 1.10000002384");
        check("9.9E-324".equals(Double.toString(1e-323)),
                "Double.toString(1e-323) must be \"9.9E-324\" — a subnormal double");

        // The canonicalising accessors. floatToIntBits COLLAPSES NaN; its Raw
        // twin does not. Both are registered; a Rust `to_bits()` is the Raw one.
        check(Float.floatToIntBits(Float.intBitsToFloat(0x7f800001)) == 0x7fc00000,
                "Float.floatToIntBits of a signalling NaN must CANONICALISE to 0x7fc00000");
        check(Float.floatToRawIntBits(Float.intBitsToFloat(0x7f800001)) == 0x7f800001,
                "Float.floatToRawIntBits must PRESERVE the payload — the twin that must not");
        check(Float.floatToIntBits(Float.intBitsToFloat(0xffc00001)) == 0x7fc00000,
                "Float.floatToIntBits must drop the NaN SIGN bit too");
        check(Double.doubleToLongBits(Double.longBitsToDouble(0x7ff0000000000001L))
                        == 0x7ff8000000000000L,
                "Double.doubleToLongBits of a signalling NaN must canonicalise");
        check(Double.doubleToRawLongBits(Double.longBitsToDouble(0x7ff0000000000001L))
                        == 0x7ff0000000000001L,
                "Double.doubleToRawLongBits must preserve the payload");
        check(Double.doubleToLongBits(OPAQUE_D[0]) == Long.MIN_VALUE,
                "Double.doubleToLongBits(-0.0) must be 0x8000000000000000 — -0.0 is NOT collapsed");

        // compare/hashCode over the two values on which == is a liar.
        check(Float.compare(OPAQUE_F[2], OPAQUE_F[2]) == 0,
                "Float.compare(NaN, NaN) must be 0 — NaN equals itself under compare");
        check(Float.compare(OPAQUE_F[0], OPAQUE_F[1]) == -1,
                "Float.compare(-0.0f, 0.0f) must be -1 — they are ORDERED");
        check(Float.compare(OPAQUE_F[1], OPAQUE_F[0]) == 1, "Float.compare(0.0f, -0.0f) must be 1");
        check(Float.compare(OPAQUE_F[2], Float.POSITIVE_INFINITY) == 1,
                "Float.compare(NaN, +Infinity) must be 1 — NaN is the largest");
        check(Float.hashCode(OPAQUE_F[0]) == 0x80000000,
                "Float.hashCode(-0.0f) must be 0x80000000");
        check(Float.hashCode(OPAQUE_F[2]) == 0x7fc00000,
                "Float.hashCode(NaN) must be the CANONICAL bits, 0x7fc00000");
        check(Double.valueOf(OPAQUE_D[0]).hashCode() == Integer.MIN_VALUE,
                "Double.valueOf(-0.0).hashCode() must be -2147483648");
        check(Double.valueOf(OPAQUE_D[2]).hashCode() == 2146959360,
                "Double.valueOf(NaN).hashCode() must be 2146959360");
        check(!Float.valueOf(OPAQUE_F[0]).equals(Float.valueOf(OPAQUE_F[1])),
                "Float.valueOf(-0.0f).equals(0.0f) must be FALSE — unlike ==");
        check(Double.valueOf(OPAQUE_D[2]).equals(Double.valueOf(OPAQUE_D[2])),
                "Double.valueOf(NaN).equals(NaN) must be TRUE — unlike ==");

        // valueOf(String): Java's grammar, and a NULL contract that differs
        // from Integer's. W7-95 pinned parseFloat/parseDouble; these are the
        // OTHER registered triples and they were never called.
        Throwable t = null;
        try {
            Float.valueOf(OPAQUE_S[28]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Float.valueOf(\"nan\") must throw NumberFormatException — Java's token is \"NaN\"");
        t = null;
        try {
            Double.valueOf(OPAQUE_S[29]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Double.valueOf(\"inf\") must throw NumberFormatException");
        t = null;
        try {
            Float.valueOf((String) null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Float.valueOf(null) must throw NullPointerException, not NFE; got " + nameOf(t));
        t = null;
        try {
            Double.valueOf((String) null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Double.valueOf(null) must throw NullPointerException, not NFE; got " + nameOf(t));
        t = null;
        try {
            Double.parseDouble(OPAQUE_S[38]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "Double.parseDouble(\"1.0dd\") must throw — one type suffix, not two");

        check(Float.floatToRawIntBits(Float.valueOf(OPAQUE_S[30]).floatValue()) == 0x41000000,
                "Float.valueOf(\"0x1p3\") must be 8.0f — hex significands are in Java's grammar");
        check(Double.doubleToRawLongBits(Double.valueOf(OPAQUE_S[39]).doubleValue())
                        == 0x3ff8000000000000L,
                "Double.valueOf(\"  1.5  \") must be 1.5 — valueOf DOES trim");
        check(Float.floatToRawIntBits(Float.parseFloat(OPAQUE_S[31])) == 0x3f800000,
                "Float.parseFloat(\"1.0f\") must be 1.0f — a TYPE SUFFIX is in Java's grammar");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[32])) == 0x3ff0000000000000L,
                "Double.parseDouble(\"1.0d\") must be 1.0");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[31])) == 0x3ff0000000000000L,
                "Double.parseDouble(\"1.0f\") must be 1.0 — an f suffix is legal for a double");
        check(Float.floatToRawIntBits(Float.parseFloat(OPAQUE_S[33])) == 0,
                "Float.parseFloat(\"1e-46\") must UNDERFLOW to +0.0f, not throw");
        check(Float.floatToRawIntBits(Float.parseFloat(OPAQUE_S[34])) == 0x7f800000,
                "Float.parseFloat(\"1e40\") must OVERFLOW to +Infinity, not throw");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[35])) == 0L,
                "Double.parseDouble(\"1e-324\") must underflow to +0.0");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[36])) == 0x7ff0000000000000L,
                "Double.parseDouble(\"1e400\") must overflow to +Infinity");
        check(Double.doubleToRawLongBits(Double.parseDouble(OPAQUE_S[37])) == Long.MIN_VALUE,
                "Double.parseDouble(\"-0.0\") must be NEGATIVE zero");

        // NEGATIVE CONTROL. The predicates over the same operands were right.
        check(Float.isNaN(OPAQUE_F[2]), "Float.isNaN(NaN) must be true");
        check(!Float.isInfinite(OPAQUE_F[3]), "Float.isInfinite(Float.MAX_VALUE) must be false");
        check(Double.isInfinite(Double.parseDouble(OPAQUE_S[36])),
                "Double.isInfinite of the overflowed parse must be true");

        sectionEnd("floatfmt", 47);
    }

    // -----------------------------------------------------------------------
    // 4. hex — java.util.HexFormat. Sixteen registered triples, ZERO invoked.
    //
    // Three hazards at once: a String-producing formatter (case and delimiter
    // policy), an ARRAY SLICE with an explicit range (Rust panics where Java
    // throws IndexOutOfBoundsException), and a builder whose withX() methods
    // must return a NEW instance rather than mutating a shared one.
    // -----------------------------------------------------------------------
    static void hex() {
        byte[] hb = { 0, (byte) 0xff, 0x0a, (byte) 0x80 };
        check(hb.length == 4 && (hb[1] & 0xff) == 0xff && (hb[3] & 0xff) == 0x80,
                "the hex fixture must be { 00, ff, 0a, 80 }");

        HexFormat f = HexFormat.of();
        check("00ff0a80".equals(f.formatHex(hb)),
                "HexFormat.of().formatHex must be lowercase and unseparated");
        check("00FF0A80".equals(f.withUpperCase().formatHex(hb)),
                "withUpperCase().formatHex must be \"00FF0A80\"");
        check("00:ff:0a:80".equals(HexFormat.ofDelimiter(":").formatHex(hb)),
                "ofDelimiter(\":\").formatHex must separate every BYTE");
        check("0x00!, 0xff!, 0x0a!, 0x80!".equals(
                        HexFormat.ofDelimiter(", ").withPrefix("0x").withSuffix("!").formatHex(hb)),
                "prefix and suffix wrap each byte, inside the delimiter");
        check("ff0a".equals(f.formatHex(hb, 1, 3)),
                "formatHex(hb, 1, 3) must be the HALF-OPEN range [1,3)");
        check("".equals(f.formatHex(hb, 0, 0)), "formatHex(hb, 0, 0) must be the empty string");
        check("".equals(f.formatHex(new byte[0])), "formatHex(empty array) must be empty");
        check("ff".equals(f.toHexDigits((byte) -1)),
                "toHexDigits((byte) -1) must be \"ff\" — two digits, unsigned");
        check("0f".equals(f.toHexDigits((byte) 0x0f)),
                "toHexDigits((byte) 0x0f) must be \"0f\" — ZERO PADDED");
        check("ffffffff".equals(f.toHexDigits(OPAQUE_I[2])),
                "toHexDigits(int -1) must be \"ffffffff\" — eight digits");
        check("ffffffffffffffff".equals(f.toHexDigits(OPAQUE_J[2])),
                "toHexDigits(long -1) must be \"ffffffffffffffff\" — sixteen digits");
        check("000000FF".equals(f.withUpperCase().toHexDigits(OPAQUE_I[7])),
                "withUpperCase().toHexDigits(255) must be \"000000FF\"");
        check("".equals(f.delimiter()), "HexFormat.of().delimiter() must be the empty string");
        check("".equals(f.prefix()), "HexFormat.of().prefix() must be the empty string");
        check("".equals(f.suffix()), "HexFormat.of().suffix() must be the empty string");
        check(!f.isUpperCase(), "HexFormat.of().isUpperCase() must be false");
        check(f.withUpperCase().isUpperCase(), "withUpperCase().isUpperCase() must be true");
        check(!f.withUpperCase().withLowerCase().isUpperCase(),
                "withLowerCase() after withUpperCase() must be false again");
        check(":".equals(HexFormat.ofDelimiter(":").delimiter()),
                "ofDelimiter(\":\").delimiter() must round-trip");
        check("-".equals(f.withDelimiter("-").delimiter()),
                "withDelimiter(\"-\").delimiter() must round-trip");
        // The builder must be IMMUTABLE: the four calls above must not have
        // mutated the receiver. A Rust body holding &mut self would fail here
        // and nowhere else.
        check(!f.isUpperCase() && "".equals(f.delimiter()),
                "HexFormat.of() must be UNCHANGED after four withX() calls on it");

        // The slice bounds. Every one of these is specified to throw.
        int[][] badRange = { { 3, 1 }, { 0, 9 }, { -1, 2 }, { 2, 9 } };
        for (int k = 0; k < badRange.length; k++) {
            Throwable t = null;
            try {
                f.formatHex(hb, badRange[k][0], badRange[k][1]);
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                    "HexFormat.formatHex(hb, " + badRange[k][0] + ", " + badRange[k][1]
                            + ") must throw IndexOutOfBoundsException, got " + nameOf(t));
        }

        sectionEnd("hex", 26);
    }

    // -----------------------------------------------------------------------
    // 5. b64 — java.util.Base64. Eleven registered triples, ZERO invoked.
    //
    // A decoder is a grammar, and W7-95's finding is that grammars are where
    // these natives fail. Java's basic decoder is LENIENT about missing padding
    // and about non-canonical trailing bits, and STRICT about a wrong alphabet
    // and about trailing data — every mainstream Rust base64 configuration
    // draws at least one of those three lines somewhere else.
    // -----------------------------------------------------------------------
    static void b64() {
        byte[] bb = { (byte) 0xfb, (byte) 0xff, (byte) 0xbf, 0x00, 0x01 };
        check(bb.length == 5 && (bb[0] & 0xff) == 0xfb,
                "the base64 fixture must start 0xfb — it forces '+' and '/' in the basic alphabet");

        check("+/+/AAE=".equals(Base64.getEncoder().encodeToString(bb)),
                "the BASIC alphabet uses '+' and '/', and pads to a multiple of 4");
        check("-_-_AAE=".equals(Base64.getUrlEncoder().encodeToString(bb)),
                "the URL alphabet uses '-' and '_' for the same two bytes");
        check("+/+/AAE".equals(Base64.getEncoder().withoutPadding().encodeToString(bb)),
                "withoutPadding() must drop the '=' and nothing else");
        check("".equals(Base64.getEncoder().encodeToString(new byte[0])),
                "encodeToString(empty) must be the empty string");
        check(Base64.getEncoder().encode(bb).length == 8,
                "encode([B) must return 8 bytes for a 5-byte input");
        check("AAEC".equals(Base64.getEncoder().encodeToString(new byte[] { 0, 1, 2 })),
                "a 3-byte input encodes to exactly 4 unpadded characters");
        check(Arrays.equals(Base64.getDecoder().decode("+/+/"), new byte[] { -5, -1, -65 }),
                "the basic decoder must accept '+' and '/'");
        check(Arrays.equals(Base64.getUrlDecoder().decode("-_-_"), new byte[] { -5, -1, -65 }),
                "the URL decoder must accept '-' and '_'");
        check(Arrays.equals(Base64.getDecoder().decode("QQ=="), new byte[] { 65 }),
                "decode(\"QQ==\") must be the single byte 65");
        check(Base64.getDecoder().decode("").length == 0, "decode(\"\") must be an empty array");
        check(Arrays.equals(Base64.getDecoder().decode(Base64.getEncoder().encodeToString(bb)), bb),
                "encode/decode must round-trip");

        // LENIENT where Rust is usually strict. These are NOT negative
        // controls — they are the rows a strict decoder gets wrong.
        check(Arrays.equals(Base64.getDecoder().decode("QQ"), new byte[] { 65 }),
                "decode(\"QQ\") must SUCCEED — the basic decoder does not require padding");
        check(Arrays.equals(Base64.getDecoder().decode("QR=="), new byte[] { 65 }),
                "decode(\"QR==\") must succeed and DISCARD the non-canonical trailing bits");
        check(Arrays.equals(Base64.getDecoder().decode("QQQ"), new byte[] { 65, 4 }),
                "decode(\"QQQ\") must yield two bytes from three unpadded characters");
        check(Arrays.equals(Base64.getMimeDecoder().decode("QQ\n=="), new byte[] { 65 }),
                "the MIME decoder must SKIP a line break inside the data");
        check(Arrays.equals(Base64.getMimeDecoder().decode("Q*Q=="), new byte[] { 65 }),
                "the MIME decoder must skip an illegal character, not throw");

        // STRICT where a lenient decoder would let it through.
        String[] bad = { "-_-_", "QQ=", "A", "QQ==X", "QQ\n==" };
        String[] badWhy = {
            "the basic decoder must REJECT the URL alphabet",
            "\"QQ=\" is a truncated padding group",
            "\"A\" is a single dangling character",
            "trailing data after the padding must be rejected",
            "the BASIC decoder must reject a line break — only the MIME one skips it",
        };
        for (int k = 0; k < bad.length; k++) {
            Throwable t = null;
            try {
                Base64.getDecoder().decode(bad[k]);
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                    badWhy[k] + " (IllegalArgumentException), got " + nameOf(t));
        }
        Throwable t = null;
        try {
            Base64.getDecoder().decode((String) null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "decode((String) null) must throw NullPointerException, got " + nameOf(t));

        // The MIME encoder's line policy is part of its contract: 76 characters
        // then CRLF, and no trailing separator.
        byte[] big = new byte[60];
        for (int k = 0; k < big.length; k++) {
            big[k] = (byte) k;
        }
        String mime = Base64.getMimeEncoder().encodeToString(big);
        check(mime.length() == 82, "the MIME encoding of 60 bytes must be 82 chars (80 + CRLF)");
        check(mime.indexOf("\r\n") == 76, "the MIME encoder must break the line after 76 chars");
        check(mime.indexOf("\r\n", 77) == -1, "there must be exactly ONE line break");
        check(Arrays.equals(Base64.getMimeDecoder().decode(mime), big),
                "the MIME encoding must round-trip through the MIME decoder");

        sectionEnd("b64", 27);
    }

    // -----------------------------------------------------------------------
    // 6. uuid — java.util.UUID. Nine registered triples, ZERO invoked.
    //
    // UUID.fromString is the interesting one, and not for the reason a reader
    // expects: Java's parser is LENIENT in a way no Rust uuid crate is. It
    // splits on '-' into exactly five groups and parses each as an unsigned hex
    // long, so "1-2-3-4-5" is accepted and a 35-character string with an
    // 11-digit node group is accepted and re-padded. A strict 36-character
    // canonical-form parser rejects both.
    // -----------------------------------------------------------------------
    static void uuid() {
        UUID u = new UUID(0x0011223344556677L, 0x8899aabbccddeeffL);
        check("00112233-4455-6677-8899-aabbccddeeff".equals(u.toString()),
                "UUID.toString must be lowercase 8-4-4-4-12");
        check(u.getMostSignificantBits() == 0x0011223344556677L,
                "getMostSignificantBits must round-trip the constructor argument");
        check(u.getLeastSignificantBits() == 0x8899aabbccddeeffL,
                "getLeastSignificantBits must round-trip the constructor argument");
        check(u.version() == 6, "version() must be bits 12-15 of the msb — 6 here");
        check(u.variant() == 2, "variant() must be 2 for a leading '8' in the lsb");
        check("00000000-0000-0001-0000-000000000001".equals(new UUID(1L, 1L).toString()),
                "toString must ZERO-PAD every group");
        check("00000000-0000-0000-0000-000000000000".equals(
                        new UUID(OPAQUE_J[3], OPAQUE_J[3]).toString()),
                "the nil UUID must print as all zeros");
        check(new UUID(OPAQUE_J[3], OPAQUE_J[3]).hashCode() == 0,
                "the nil UUID's hashCode must be 0 — msb^lsb folded");
        UUID h = new UUID(0x0123456789abcdefL, 0x1122334455667788L);
        check(h.hashCode() == -858993596,
                "UUID.hashCode must be (msb^lsb) folded to 32 bits, not an object identity");
        check("01234567-89ab-cdef-1122-334455667788".equals(h.toString()),
                "the second fixture must print its own digits");
        check(h.version() == 12, "version() must report 12 even though no such version exists");
        check(h.variant() == 0, "variant() must be 0 for a leading '1' in the lsb");

        UUID v1 = UUID.fromString("f81d4fae-7dec-11d0-a765-00a0c91e6bf6");
        check(v1.getMostSignificantBits() == 0xf81d4fae7dec11d0L,
                "fromString must parse the msb of the RFC 4122 example");
        check(v1.getLeastSignificantBits() == 0xa76500a0c91e6bf6L,
                "fromString must parse the lsb of the RFC 4122 example");
        check(v1.version() == 1, "the RFC 4122 example is version 1");
        check(v1.variant() == 2, "the RFC 4122 example is variant 2");
        check(v1.hashCode() == -343263960, "the RFC 4122 example's hashCode is fixed");
        check("00112233-4455-6677-8899-aabbccddeeff".equals(
                        UUID.fromString("00112233-4455-6677-8899-AABBCCDDEEFF").toString()),
                "fromString must accept UPPERCASE and toString must normalise to lower");
        check(UUID.fromString("00112233-4455-6677-8899-aabbccddeeff").equals(u),
                "fromString must produce a value equal to the constructed UUID");
        check(!u.equals(null), "UUID.equals(null) must be false, not a panic");

        // LENIENT. Both of these are accepted by the JDK and rejected by every
        // strict canonical-form parser.
        check("00000001-0002-0003-0004-000000000005".equals(UUID.fromString("1-2-3-4-5").toString()),
                "fromString(\"1-2-3-4-5\") must be ACCEPTED — five hex groups, re-padded");
        check("00112233-4455-6677-8899-0aabbccddeef".equals(
                        UUID.fromString("00112233-4455-6677-8899-aabbccddeef").toString()),
                "a 35-character form with an 11-digit node group must be accepted and re-padded");

        // STRICT.
        String[] bad = {
            "00112233445566778899aabbccddeeff", "", "00112233-4455-6677-8899-aabbccddeeff-0",
        };
        String[] badWhy = {
            "a form with NO dashes must be rejected",
            "the empty string must be rejected",
            "a SIXTH group must be rejected",
        };
        for (int k = 0; k < bad.length; k++) {
            Throwable t = null;
            try {
                UUID.fromString(bad[k]);
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                    "UUID.fromString: " + badWhy[k] + " (IllegalArgumentException), got "
                            + nameOf(t));
        }
        Throwable t = null;
        try {
            UUID.fromString("zzzzzzzz-4455-6677-8899-aabbccddeeff");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(t)),
                "a non-hex group must throw NumberFormatException — NOT IllegalArgument, because"
                        + " the group count passed and the DIGITS are what failed; got "
                        + nameOf(t));

        // randomUUID is nondeterministic by construction, so only its
        // structural invariants are assertable.
        UUID r1 = UUID.randomUUID();
        UUID r2 = UUID.randomUUID();
        check(r1.version() == 4, "randomUUID must set version 4");
        check(r1.variant() == 2, "randomUUID must set variant 2 (IETF)");
        check(r1.toString().length() == 36, "randomUUID().toString() must be 36 characters");
        check(!r1.equals(r2), "two randomUUIDs must differ");

        sectionEnd("uuid", 30);
    }

    // -----------------------------------------------------------------------
    // 7. random — java.util.Random. Twenty-two registered triples, ZERO invoked.
    //
    // This family is not "probably right": every value below is FIXED BY
    // JAVADOC. java.util.Random specifies its 48-bit LCG, the exact seed
    // scramble, the exact next(bits) recurrence, the rejection loop in
    // nextInt(bound), the power-of-two special case, and the polar method with
    // its CACHED SECOND VALUE for nextGaussian. A body that reaches for any
    // Rust RNG produces different numbers from the same seed, and
    // [nextGaus] — random-nextgaussian-discarded-the-cached-partner — is a
    // PRIOR FINDING in exactly this family, which is the strongest possible
    // argument that these twenty-two triples deserved a vector.
    // -----------------------------------------------------------------------
    static void random() {
        Random r = new Random(OPAQUE_J[8]);
        check(r.nextInt() == -1170105035, "new Random(42).nextInt() #1 is specified");
        check(r.nextInt() == 234785527, "new Random(42).nextInt() #2 is specified");
        check(r.nextInt() == -1360544799, "new Random(42).nextInt() #3 is specified");

        Random r2 = new Random(OPAQUE_J[8]);
        check(r2.nextLong() == -5025562857975149833L, "new Random(42).nextLong() #1 is specified");
        check(r2.nextLong() == -5843495416241995736L, "new Random(42).nextLong() #2 is specified");

        Random r3 = new Random(OPAQUE_J[8]);
        check(Double.doubleToRawLongBits(r3.nextDouble()) == 0x3fe74833a06ff457L,
                "new Random(42).nextDouble() #1 is specified, to the bit");
        check(Double.doubleToRawLongBits(r3.nextDouble()) == 0x3fe5dcf778622e01L,
                "new Random(42).nextDouble() #2 is specified, to the bit");

        Random r4 = new Random(OPAQUE_J[8]);
        check(Float.floatToRawIntBits(r4.nextFloat()) == 0x3f3a419d,
                "new Random(42).nextFloat() #1 is specified, to the bit");

        Random r5 = new Random(OPAQUE_J[8]);
        boolean[] wantBool = { true, false, true, false, false };
        for (int k = 0; k < wantBool.length; k++) {
            check(r5.nextBoolean() == wantBool[k],
                    "new Random(42).nextBoolean() #" + (k + 1) + " is specified");
        }

        Random r6 = new Random(OPAQUE_J[8]);
        int[] want100 = { 30, 63, 48, 84, 70 };
        for (int k = 0; k < want100.length; k++) {
            check(r6.nextInt(100) == want100[k],
                    "new Random(42).nextInt(100) #" + (k + 1) + " is specified — the modulo"
                            + " rejection loop is part of the contract");
        }

        Random r7 = new Random(OPAQUE_J[8]);
        int[] want16 = { 11, 0, 10, 0, 4 };
        for (int k = 0; k < want16.length; k++) {
            check(r7.nextInt(OPAQUE_I[11]) == want16[k],
                    "new Random(42).nextInt(16) #" + (k + 1) + " is specified — a POWER OF TWO"
                            + " takes a different, also-specified path");
        }

        // nextGaussian's cached partner. [nextGaus]: a body that computes the
        // polar pair and discards the second value produces the right #1 and
        // the wrong #2, so #2 is the load-bearing row.
        Random r8 = new Random(OPAQUE_J[8]);
        check(Double.doubleToRawLongBits(r8.nextGaussian()) == 0x3ff2453e82115d86L,
                "new Random(42).nextGaussian() #1 is specified, to the bit");
        check(Double.doubleToRawLongBits(r8.nextGaussian()) == 0x3fed6bca38120847L,
                "nextGaussian() #2 must be the CACHED PARTNER of #1, not a fresh polar pair");
        check(Double.doubleToRawLongBits(r8.nextGaussian()) == 0xbfee654eb7a040c2L,
                "nextGaussian() #3 restarts the polar loop");

        Random r9 = new Random(OPAQUE_J[8]);
        byte[] nb = new byte[7];
        r9.nextBytes(nb);
        check(Arrays.equals(nb, new byte[] { 53, -99, 65, -70, -9, -118, -2 }),
                "new Random(42).nextBytes(new byte[7]) is specified, byte for byte — note the"
                        + " length is NOT a multiple of 4, which is where the tail loop lives");

        check(new Random(OPAQUE_J[3]).nextInt() == -1155484576,
                "new Random(0).nextInt() is specified — seed 0 still gets SCRAMBLED");
        check(new Random(OPAQUE_J[2]).nextInt() == 1155099827,
                "new Random(-1).nextInt() is specified");
        check(new Random(Long.MAX_VALUE).nextInt() == 1155099827,
                "new Random(MAX_VALUE).nextInt() equals the seed -1 case — only 48 bits survive"
                        + " the scramble, which is the tell that the mask is applied");
        Random r11 = new Random(OPAQUE_J[4]);
        r11.setSeed(OPAQUE_J[8]);
        check(r11.nextInt() == -1170105035,
                "setSeed(42) must RESET the stream, including the Gaussian cache");
        check(new Random(OPAQUE_J[8]).nextInt(1) == 0, "nextInt(1) must be 0, always");

        int[] badBound = { 0, -5 };
        for (int k = 0; k < badBound.length; k++) {
            Throwable t = null;
            try {
                new Random(OPAQUE_J[4]).nextInt(badBound[k]);
            } catch (Throwable x) {
                t = x;
            }
            check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                    "Random.nextInt(" + badBound[k] + ") must throw IllegalArgumentException, got "
                            + nameOf(t));
        }

        sectionEnd("random", 34);
    }

    // -----------------------------------------------------------------------
    // 8. strfmt — String.format / formatted / lines / indent / chars / replace
    //    / matches.
    //
    // W7-95 named String.format explicitly as registered-and-never-invoked, and
    // stated the reason it matters: "a formatter is a grammar, and this
    // record's finding is that grammars are where these natives fail". The
    // rest of this block is the other never-invoked String triples.
    //
    // chars() is the direct sibling of the codePoints() defect W7-95 pinned: a
    // 55357 in a chars() answer is CORRECT (it is a code UNIT), and a 65533 is
    // the same UTF-8 pipe showing through. The two rows below distinguish them.
    //
    // Every %-conversion whose output depends on locale is asked through the
    // (Locale, String, Object[]) overload with Locale.ROOT, because the
    // (String, Object[]) overload reads the DEFAULT locale and would make this
    // vector host-dependent. Both overloads are registered triples, so both are
    // exercised — the no-locale one only on conversions that cannot vary.
    // -----------------------------------------------------------------------
    static void strfmt() {
        check("-0.0".equals(String.format(Locale.ROOT, "%.1f", Double.valueOf(OPAQUE_D[0]))),
                "String.format(\"%.1f\", -0.0) must keep the SIGN of negative zero");
        check("0.000000e+00".equals(String.format(Locale.ROOT, "%e", Double.valueOf(OPAQUE_D[1]))),
                "%e must be six fraction digits and a TWO-digit exponent");
        check("0.000100000".equals(String.format(Locale.ROOT, "%g", Double.valueOf(0.0001))),
                "%g below the exponent threshold must be fixed-point with 6 significant digits");
        check("1.23457e+06".equals(String.format(Locale.ROOT, "%g", Double.valueOf(1234567.0))),
                "%g above the threshold must switch to scientific and ROUND");
        check("-2147483648".equals(String.format(Locale.ROOT, "%d", Integer.valueOf(OPAQUE_I[0]))),
                "%d over Integer.MIN_VALUE must not overflow while negating");
        check("ffffffff".equals(String.format(Locale.ROOT, "%x", Integer.valueOf(OPAQUE_I[2]))),
                "%x of an int -1 must be eight digits, UNSIGNED");
        check("ffffffffffffffff".equals(String.format(Locale.ROOT, "%x", Long.valueOf(OPAQUE_J[2]))),
                "%x of a long -1 must be sixteen digits");
        check("1,234,567".equals(String.format(Locale.ROOT, "%,d", Integer.valueOf(1234567))),
                "the ',' flag must group by three under Locale.ROOT");
        check("0003.142".equals(String.format(Locale.ROOT, "%08.3f", Double.valueOf(OPAQUE_D[4]))),
                "'0' width padding must go BETWEEN the sign position and the digits");
        check("42      |".equals(String.format(Locale.ROOT, "%-8d|", Integer.valueOf(42))),
                "'-' must left-justify inside the width");
        check("+42".equals(String.format(Locale.ROOT, "%+d", Integer.valueOf(42))),
                "'+' must force the sign on a positive value");
        check("(42)".equals(String.format(Locale.ROOT, "%(d", Integer.valueOf(-42))),
                "'(' must render a negative value in parentheses");
        check("1.234,50".equals(String.format(Locale.GERMANY, "%,.2f", Double.valueOf(OPAQUE_D[5]))),
                "the (Locale, ...) overload must actually USE the locale: German swaps the"
                        + " grouping and decimal separators");
        check("NaN".equals(String.format(Locale.ROOT, "%.2f", Double.valueOf(OPAQUE_D[2]))),
                "%.2f of NaN must be \"NaN\", with the precision IGNORED");
        check("Infinity".equals(
                        String.format(Locale.ROOT, "%.2f", Double.valueOf(Double.POSITIVE_INFINITY))),
                "%.2f of +Infinity must be \"Infinity\"");
        check("1".equals(String.format(Locale.ROOT, "%.0f", Double.valueOf(OPAQUE_D[6]))),
                "%.0f of 0.5 must be \"1\" — HALF_UP, not the even-rounding of the FP unit");
        check("2".equals(String.format(Locale.ROOT, "%.0f", Double.valueOf(OPAQUE_D[3]))),
                "%.0f of 1.5 must be \"2\"");
        check("3".equals(String.format(Locale.ROOT, "%.0f", Double.valueOf(OPAQUE_D[7]))),
                "%.0f of 2.5 must be \"3\" — HALF_UP again, where HALF_EVEN would say 2");
        // Locale-independent conversions, asked through the OTHER registered
        // overload so it is exercised too.
        check("null".equals(String.format("%s", (Object) null)),
                "%s of a null argument must be the string \"null\"");
        check("false".equals(String.format("%b", (Object) null)),
                "%b of null must be \"false\" — %b does NOT mean Boolean.valueOf");
        check("true".equals(String.format("%b", "x")),
                "%b of any non-null non-Boolean must be \"true\"");
        check("AB".equals(String.format("%S", "ab")), "%S must upper-case the conversion's output");
        check("b a".equals(String.format("%2$s %1$s", "a", "b")),
                "explicit argument indices must reorder");
        check("100%".equals(String.format("100%%")), "%% must emit one literal percent");
        check(String.format("%n").equals(System.lineSeparator()),
                "%n must be the PLATFORM line separator");
        check("x!".equals("%s!".formatted("x")),
                "String.formatted is its own registered triple and must agree with format");

        String[] badFmt = { "%q", "%d" };
        String[] wantEx = {
            "java.util.UnknownFormatConversionException", "java.util.MissingFormatArgumentException",
        };
        for (int k = 0; k < badFmt.length; k++) {
            Throwable t = null;
            try {
                String.format(Locale.ROOT, badFmt[k]);
            } catch (Throwable x) {
                t = x;
            }
            check(wantEx[k].equals(nameOf(t)),
                    "String.format(\"" + badFmt[k] + "\") must throw " + wantEx[k] + ", got "
                            + nameOf(t));
        }
        Throwable t = null;
        try {
            String.format(Locale.ROOT, "%d", "x");
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.IllegalFormatConversionException".equals(nameOf(t)),
                "String.format(\"%d\", \"x\") must throw IllegalFormatConversionException, got "
                        + nameOf(t));

        // chars(): UTF-16 code UNITS, so the surrogates stay unpaired AND
        // unreplaced. This is the codePoints() defect's sibling triple.
        String pair = "a" + new String(Character.toChars(OPAQUE_CP[9])) + "b";
        String loneHigh = "x\ud800y";
        check(pair.length() == 4 && pair.charAt(1) == 0xd83d && pair.charAt(2) == 0xde00,
                "the surrogate-pair fixture must be a + U+D83D U+DE00 + b");
        check(pair.chars().count() == 4,
                "\"a<U+1F600>b\".chars() must yield FOUR code units — chars() does not pair");
        check(pair.chars().toArray()[1] == 55357,
                "chars()[1] must be the raw high surrogate 55357, not 128512 and not 65533");
        check(loneHigh.chars().toArray()[1] == 55296,
                "chars() over an UNPAIRED high surrogate must be 55296, not U+FFFD");

        // lines(): the terminator rules, including the empty string.
        check("".lines().count() == 0, "\"\".lines() must be EMPTY — zero lines, not one");
        check("a\n\nb".lines().count() == 3, "an empty line between two lines must be preserved");
        check("a\r\nb".lines().count() == 2, "CRLF is ONE terminator");
        check("a\rb".lines().count() == 2, "a bare CR is also a terminator");
        check("a\n".lines().count() == 1, "a TRAILING terminator must not add an empty line");
        check("a\n\n".lines().count() == 2, "two trailing terminators leave one empty line");

        // indent(): always normalises terminators and always appends one.
        check("  a\n  b\n".equals("a\nb".indent(2)), "indent(2) must prefix every line and append \\n");
        check("a\n".equals("a".indent(0)),
                "indent(0) must still APPEND a line terminator — it is not a no-op");
        check(" a\n".equals("  a".indent(-1)), "a negative indent must strip leading white space");
        check(" a\n b\n".equals("a\r\nb".indent(1)),
                "indent must NORMALISE CRLF to LF while it is at it");
        check("".equals("".indent(1)), "\"\".indent(1) must stay empty — no lines to indent");

        // repeat / replace / replaceAll / matches.
        check("".equals("ab".repeat(OPAQUE_I[3])), "repeat(0) must be the empty string");
        check("".equals("".repeat(5)), "\"\".repeat(5) must be the empty string");
        check("-a-b-c-".equals("abc".replace("", "-")),
                "replace with an EMPTY target must insert at every boundary, ends included");
        check("-".equals("".replace("", "-")), "\"\".replace(\"\", \"-\") must be \"-\"");
        check("XbX".equals("aaabaa".replaceAll("a+", "X")), "replaceAll must replace every match");
        check("Xbaa".equals("aaabaa".replaceFirst("a+", "X")),
                "replaceFirst must replace exactly one");
        check("ba".equals("ab".replaceAll("(a)(b)", "$2$1")),
                "group references in the replacement must be honoured");
        String astral = new String(Character.toChars(OPAQUE_CP[9]));
        check(astral.matches("."),
                "\".\" must match one CODE POINT — a surrogate pair is one dot, not two");
        check("\ud800".matches("."), "\".\" must also match a LONE surrogate");
        check("abc".matches("a.c"), "String.matches must anchor at both ends and match here");
        check(!"abc".matches("a"), "String.matches must be a FULL match, so \"a\" must not match");
        t = null;
        try {
            "x".replaceAll("[", "y");
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.regex.PatternSyntaxException".equals(nameOf(t)),
                "an unterminated character class must throw PatternSyntaxException, got "
                        + nameOf(t));
        t = null;
        try {
            "ab".replaceAll("(a)", "$9");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "a group reference past the group count must throw IndexOutOfBoundsException, got "
                        + nameOf(t));

        check("null".equals(String.valueOf((Object) null)),
                "String.valueOf((Object) null) must be \"null\", not a NullPointerException");
        check("-2147483648".equals(String.valueOf(OPAQUE_I[0])),
                "String.valueOf(Integer.MIN_VALUE) must not overflow while negating");
        check("abcd".transform(String::length).intValue() == 4,
                "String.transform must apply the function and return its result");

        sectionEnd("strfmt", 60);
    }

    // -----------------------------------------------------------------------
    // 9. bounds — THE INDEX-PANIC FAMILY. Prints a step line before each call.
    //
    // Rust panics on an out-of-bounds slice index, and a panic inside a native
    // is not a Java throwable: it terminates the VM. Every call in this block
    // is an indexed accessor on a registered Intrinsic whose Java contract is
    // to THROW a specific exception class — AtomicReferenceArray (nine triples,
    // zero invoked), CharBuffer and the ByteBufferAsCharBuffer views (fourteen
    // triples, zero invoked), and the String code-point accessors at their
    // bounds, which W7-95 measured for the BEGIN>END case and not for the
    // negative or past-the-end ones.
    //
    // The exact class is asserted, not a superclass: ArrayIndexOutOfBounds and
    // StringIndexOutOfBounds are both specified here, and a body that throws
    // the generic IndexOutOfBoundsException for all of them is wrong in a way
    // an `instanceof` check would not see.
    // -----------------------------------------------------------------------
    static void bounds() {
        AtomicReferenceArray<String> ara = new AtomicReferenceArray<>(OPAQUE_I[6]);
        check(ara.length() == 3, "AtomicReferenceArray(3).length() must be 3");
        check(ara.get(OPAQUE_I[3]) == null, "a fresh AtomicReferenceArray element must be null");
        ara.set(OPAQUE_I[3], "a");
        check("a".equals(ara.get(OPAQUE_I[3])), "set then get must round-trip");
        check("a".equals(ara.getAndSet(OPAQUE_I[3], "b")), "getAndSet must return the OLD value");
        check(ara.compareAndSet(OPAQUE_I[3], "b", "c"),
                "compareAndSet must succeed on the current value");
        check(!ara.compareAndSet(OPAQUE_I[3], "zz", "d"),
                "compareAndSet must fail on a stale expected value");
        check("c".equals(ara.get(OPAQUE_I[3])), "the failed CAS must not have written");
        check(ara.compareAndSet(OPAQUE_I[4], null, "x"),
                "compareAndSet against a NULL expected value must succeed on a fresh slot");
        ara.lazySet(OPAQUE_I[5], "y");
        check("y".equals(ara.get(OPAQUE_I[5])), "lazySet must be visible to a same-thread get");
        check(new AtomicReferenceArray<String>(OPAQUE_I[3]).length() == 0,
                "a zero-length AtomicReferenceArray is legal");

        step("bounds", "AtomicReferenceArray.get(-1)");
        Throwable t = null;
        try {
            ara.get(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "AtomicReferenceArray.get(-1) must throw ArrayIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "AtomicReferenceArray.get(len)");
        t = null;
        try {
            ara.get(OPAQUE_I[6]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "AtomicReferenceArray.get(3) on a length-3 array must throw, got " + nameOf(t));
        step("bounds", "AtomicReferenceArray.set(len)");
        t = null;
        try {
            ara.set(OPAQUE_I[6], "x");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "AtomicReferenceArray.set(3, x) must throw — a WRITE past the end, got "
                        + nameOf(t));
        step("bounds", "AtomicReferenceArray.compareAndSet(9)");
        t = null;
        try {
            ara.compareAndSet(OPAQUE_I[13], null, "x");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "AtomicReferenceArray.compareAndSet(9, ...) must throw, got " + nameOf(t));
        step("bounds", "AtomicReferenceArray.getAndSet(-1)");
        t = null;
        try {
            ara.getAndSet(OPAQUE_I[2], "x");
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "AtomicReferenceArray.getAndSet(-1, x) must throw, got " + nameOf(t));
        step("bounds", "new AtomicReferenceArray(-1)");
        t = null;
        try {
            sink = new AtomicReferenceArray<String>(OPAQUE_I[2]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NegativeArraySizeException".equals(nameOf(t)),
                "new AtomicReferenceArray(-1) must throw NegativeArraySizeException — NOT"
                        + " IllegalArgument and NOT a capacity panic; got " + nameOf(t));

        // CharBuffer: get(int) is ABSOLUTE and must not move the position;
        // get() is relative and must.
        CharBuffer cb = CharBuffer.wrap("abcd");
        check(cb.get(OPAQUE_I[4]) == 'b', "CharBuffer.get(1) must be absolute");
        check(cb.charAt(OPAQUE_I[4]) == 'b', "CharBuffer.charAt(1) must agree before any read");
        check(cb.get() == 'a', "CharBuffer.get() must be relative and start at the position");
        check(cb.charAt(OPAQUE_I[4]) == 'c',
                "charAt is relative to the POSITION, so it must move after get() — this is the"
                        + " row that separates charAt from get(int)");

        step("bounds", "CharBuffer.get(9)");
        t = null;
        try {
            cb.get(OPAQUE_I[13]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "CharBuffer.get(9) must throw IndexOutOfBoundsException, got " + nameOf(t));
        step("bounds", "CharBuffer.get(-1)");
        t = null;
        try {
            cb.get(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "CharBuffer.get(-1) must throw IndexOutOfBoundsException, got " + nameOf(t));
        step("bounds", "CharBuffer.charAt(9)");
        t = null;
        try {
            cb.charAt(OPAQUE_I[13]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "CharBuffer.charAt(9) must throw IndexOutOfBoundsException, got " + nameOf(t));

        // The ByteBufferAsCharBuffer views: same accessors, and ENDIANNESS is
        // the observable that separates the two registered classes.
        ByteBuffer raw = ByteBuffer.allocate(8);
        raw.putChar(0, 'A');
        raw.putChar(2, 'B');
        CharBuffer be = raw.order(ByteOrder.BIG_ENDIAN).asCharBuffer();
        check(be.get(OPAQUE_I[3]) == 'A', "the BIG_ENDIAN char view must read 'A'");
        check(be.charAt(OPAQUE_I[4]) == 'B', "the BIG_ENDIAN char view's charAt(1) must be 'B'");
        check(!be.hasArray(), "a ByteBuffer-backed char view must report hasArray() == false");
        check("AB  ".equals(be.toString()),
                "the view's toString must be its four chars, trailing NULs included");
        check(be.remaining() == 4, "eight bytes make four chars");
        CharBuffer le = raw.order(ByteOrder.LITTLE_ENDIAN).asCharBuffer();
        check(le.get(OPAQUE_I[3]) == 0x4100,
                "the LITTLE_ENDIAN view over the SAME bytes must read 0x4100, not 'A' — the two"
                        + " ByteBufferAsCharBuffer classes are registered separately and must"
                        + " differ here");
        step("bounds", "ByteBufferAsCharBuffer.get(9)");
        t = null;
        try {
            be.get(OPAQUE_I[13]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "the char view's get(9) must throw IndexOutOfBoundsException, got " + nameOf(t));

        // String's code-point accessors at their bounds. W7-95 measured the
        // begin>end case; these are the other three.
        step("bounds", "String.codePointAt(len)");
        t = null;
        try {
            "ab".codePointAt(OPAQUE_I[5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".codePointAt(2) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "String.codePointAt(-1)");
        t = null;
        try {
            "ab".codePointAt(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".codePointAt(-1) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "String.offsetByCodePoints(-1, 1)");
        t = null;
        try {
            "ab".offsetByCodePoints(OPAQUE_I[2], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".offsetByCodePoints(-1, 1) must throw IndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "String.codePointCount(-1, 2)");
        t = null;
        try {
            "ab".codePointCount(OPAQUE_I[2], OPAQUE_I[5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".codePointCount(-1, 2) must throw IndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "String.regionMatches(null)");
        t = null;
        try {
            "ab".regionMatches(OPAQUE_I[3], null, OPAQUE_I[3], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "String.regionMatches with a null other must throw NullPointerException — the"
                        + " NEGATIVE-length row W7-95 pinned returns true, so this one proves the"
                        + " null check happens FIRST; got " + nameOf(t));

        sectionEnd("bounds", 35);
    }

    // -----------------------------------------------------------------------
    // 10. strictExact — the nineteen StrictMath integral forms W7-95 called
    //     "probably green, unverified", plus abs at MIN_VALUE.
    //
    // W7-95 measured Math's Exact family and found it correct, and reasoned
    // that the StrictMath twins share those bodies. That is a reasonable
    // inference and it is not a measurement — and [dup-fix] / [2twins] are the
    // two standing records about exactly this shape in this tree. Verified
    // here.
    //
    // Math.abs(Integer.MIN_VALUE) is the row that does not follow from the
    // inference: Java specifies that it WRAPS and returns MIN_VALUE, while
    // Rust's `i32::abs` panics on that input under overflow checks. It is
    // grouped with divmod's hazard, not with the Exact family's, which is why
    // this block runs second-to-last.
    // -----------------------------------------------------------------------
    static void strictExact() {
        check(StrictMath.toIntExact(2147483647L) == Integer.MAX_VALUE,
                "StrictMath.toIntExact(2147483647L) must succeed");
        check(StrictMath.toIntExact(-2147483648L) == Integer.MIN_VALUE,
                "StrictMath.toIntExact(-2147483648L) must succeed at the boundary");
        check(StrictMath.addExact(OPAQUE_I[1] - 1, OPAQUE_I[4]) == Integer.MAX_VALUE,
                "StrictMath.addExact(MAX-1, 1) must succeed at the boundary");
        check(StrictMath.multiplyExact(OPAQUE_I[5], OPAQUE_I[6]) == 6,
                "StrictMath.multiplyExact(2, 3) must be 6");
        check(StrictMath.min(OPAQUE_I[0], OPAQUE_I[1]) == Integer.MIN_VALUE,
                "StrictMath.min(MIN_INT, MAX_INT) must be MIN_INT");
        check(StrictMath.max(OPAQUE_I[0], OPAQUE_I[1]) == Integer.MAX_VALUE,
                "StrictMath.max(MIN_INT, MAX_INT) must be MAX_INT");
        check(StrictMath.min(OPAQUE_J[0], OPAQUE_J[1]) == Long.MIN_VALUE,
                "StrictMath.min(MIN_LONG, MAX_LONG) must be MIN_LONG");
        check(StrictMath.max(OPAQUE_J[0], OPAQUE_J[1]) == Long.MAX_VALUE,
                "StrictMath.max(MIN_LONG, MAX_LONG) must be MAX_LONG");
        check(Double.doubleToRawLongBits(StrictMath.abs(OPAQUE_D[0])) == 0L,
                "StrictMath.abs(-0.0) must be POSITIVE zero — the bits, not ==");
        check(Float.floatToRawIntBits(StrictMath.abs(OPAQUE_F[0])) == 0,
                "StrictMath.abs(-0.0f) must be positive zero");

        // multiplyHigh / unsignedMultiplyHigh: the high 64 bits of a 128-bit
        // product, and the signed/unsigned pair must NOT agree at -1.
        check(Math.multiplyHigh(OPAQUE_J[0], OPAQUE_J[0]) == 4611686018427387904L,
                "Math.multiplyHigh(MIN_LONG, MIN_LONG) must be 2^62");
        check(Math.multiplyHigh(OPAQUE_J[2], OPAQUE_J[2]) == 0L,
                "Math.multiplyHigh(-1, -1) must be 0 — the SIGNED product is 1");
        check(Math.unsignedMultiplyHigh(OPAQUE_J[2], OPAQUE_J[2]) == -2L,
                "Math.unsignedMultiplyHigh(-1, -1) must be -2 — the UNSIGNED twin disagrees");
        check(Math.multiplyHigh(OPAQUE_J[1], OPAQUE_J[1]) == 4611686018427387903L,
                "Math.multiplyHigh(MAX_LONG, MAX_LONG) must be 2^62 - 1");
        check(StrictMath.multiplyHigh(OPAQUE_J[0], OPAQUE_J[0]) == 4611686018427387904L,
                "StrictMath.multiplyHigh must agree with Math's");
        check(StrictMath.multiplyHigh(OPAQUE_J[2], OPAQUE_J[2]) == 0L,
                "StrictMath.multiplyHigh(-1, -1) must be 0");
        check(StrictMath.unsignedMultiplyHigh(OPAQUE_J[2], OPAQUE_J[2]) == -2L,
                "StrictMath.unsignedMultiplyHigh(-1, -1) must be -2");

        // Every Exact form at its overflow boundary. All thirteen must be a
        // catchable ArithmeticException, never a panic.
        Throwable t;
        t = null;
        try {
            StrictMath.addExact(OPAQUE_I[1], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.addExact(MAX_INT, 1) must throw ArithmeticException, got " + nameOf(t));
        t = null;
        try {
            StrictMath.subtractExact(OPAQUE_I[0], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.subtractExact(MIN_INT, 1) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.multiplyExact(OPAQUE_I[0], OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.multiplyExact(MIN_INT, -1) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.negateExact(OPAQUE_I[0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.negateExact(MIN_INT) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.incrementExact(OPAQUE_I[1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.incrementExact(MAX_INT) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.decrementExact(OPAQUE_I[0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.decrementExact(MIN_INT) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.toIntExact(OPAQUE_J[0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.toIntExact(MIN_LONG) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.addExact(OPAQUE_J[1], OPAQUE_J[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.addExact(MAX_LONG, 1L) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.subtractExact(OPAQUE_J[0], OPAQUE_J[4]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.subtractExact(MIN_LONG, 1L) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.multiplyExact(OPAQUE_J[0], OPAQUE_J[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.multiplyExact(MIN_LONG, -1L) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.negateExact(OPAQUE_J[0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.negateExact(MIN_LONG) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.incrementExact(OPAQUE_J[1]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.incrementExact(MAX_LONG) must throw, got " + nameOf(t));
        t = null;
        try {
            StrictMath.decrementExact(OPAQUE_J[0]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.decrementExact(MIN_LONG) must throw, got " + nameOf(t));

        // abs at MIN_VALUE: Java WRAPS. Rust's i32::abs / i64::abs panic on
        // this input under overflow checks, so these four rows carry the same
        // hazard as divmod and are placed last in this block.
        step("strictExact", "Math.abs(Integer.MIN_VALUE)");
        check(Math.abs(OPAQUE_I[0]) == Integer.MIN_VALUE,
                "Math.abs(Integer.MIN_VALUE) must WRAP to Integer.MIN_VALUE, per its javadoc");
        step("strictExact", "StrictMath.abs(Integer.MIN_VALUE)");
        check(StrictMath.abs(OPAQUE_I[0]) == Integer.MIN_VALUE,
                "StrictMath.abs(Integer.MIN_VALUE) must wrap too");
        step("strictExact", "Math.abs(Long.MIN_VALUE)");
        check(Math.abs(OPAQUE_J[0]) == Long.MIN_VALUE,
                "Math.abs(Long.MIN_VALUE) must wrap to Long.MIN_VALUE");
        step("strictExact", "StrictMath.abs(Long.MIN_VALUE)");
        check(StrictMath.abs(OPAQUE_J[0]) == Long.MIN_VALUE,
                "StrictMath.abs(Long.MIN_VALUE) must wrap too");

        sectionEnd("strictExact", 34);
    }

    // -----------------------------------------------------------------------
    // 11. divmod — MUST RUN LAST. W7-95's own prediction, executed.
    //
    // W7-95 named these two triples and did not call them:
    //
    //   "StrictMath.floorDiv(JJ)J and StrictMath.floorMod(JJ)J are registered
    //    onto the SAME native_math_floor_div_long / native_math_floor_mod_long
    //    bodies that abort the VM for Math, so they are near-certainly two more
    //    VM-fatal triples that this probe simply did not call."
    //
    // On a VM that still panics there, the first step line below is the last
    // output of the process. Every other family has already reported.
    // -----------------------------------------------------------------------
    static void divmod() {
        // The non-overflowing rounding first, so a panic later cannot be
        // confused with the body being wrong in the interior of its domain.
        check(StrictMath.floorDiv(OPAQUE_J[6], OPAQUE_J[5]) == -4L,
                "StrictMath.floorDiv(-7L, 2L) must be -4 — rounds toward negative infinity");
        check(StrictMath.floorMod(OPAQUE_J[6], OPAQUE_J[5]) == 1L,
                "StrictMath.floorMod(-7L, 2L) must be 1");
        check(StrictMath.floorMod(7L, OPAQUE_J[9]) == -1L,
                "StrictMath.floorMod(7L, -2L) must be -1 — the sign follows the DIVISOR");

        Throwable t = null;
        try {
            StrictMath.floorDiv(OPAQUE_J[4], OPAQUE_J[3]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.floorDiv(1L, 0L) must throw ArithmeticException, got " + nameOf(t));
        t = null;
        try {
            StrictMath.floorMod(OPAQUE_J[4], OPAQUE_J[3]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(t)),
                "StrictMath.floorMod(1L, 0L) must throw ArithmeticException, got " + nameOf(t));

        // The opcode, one dispatch away from the natives below. Same JVMS rule.
        check(OPAQUE_J[0] / OPAQUE_J[2] == Long.MIN_VALUE,
                "the ldiv OPCODE must wrap MIN_LONG / -1 to MIN_LONG (JVMS 6.5)");
        check(OPAQUE_J[0] % OPAQUE_J[2] == 0L, "the lrem OPCODE must answer 0 for MIN_LONG % -1");

        // From here on, every call is a candidate to abort the VM.
        step("divmod", "StrictMath.floorDiv(Long.MIN_VALUE, -1L)");
        check(StrictMath.floorDiv(OPAQUE_J[0], OPAQUE_J[2]) == Long.MIN_VALUE,
                "StrictMath.floorDiv(Long.MIN_VALUE, -1L) must WRAP to Long.MIN_VALUE");
        step("divmod", "StrictMath.floorMod(Long.MIN_VALUE, -1L)");
        check(StrictMath.floorMod(OPAQUE_J[0], OPAQUE_J[2]) == 0L,
                "StrictMath.floorMod(Long.MIN_VALUE, -1L) must be 0");
        step("divmod", "StrictMath.floorDiv(Integer.MIN_VALUE, -1)");
        check(StrictMath.floorDiv(OPAQUE_I[0], OPAQUE_I[2]) == Integer.MIN_VALUE,
                "StrictMath.floorDiv(Integer.MIN_VALUE, -1) must wrap");
        step("divmod", "StrictMath.floorMod(Integer.MIN_VALUE, -1)");
        check(StrictMath.floorMod(OPAQUE_I[0], OPAQUE_I[2]) == 0,
                "StrictMath.floorMod(Integer.MIN_VALUE, -1) must be 0");
        step("divmod", "Math.floorMod(int widened to long, -1L)");
        check(Math.floorMod(OPAQUE_I[0], OPAQUE_J[2]) == 0L,
                "Math.floorMod(Integer.MIN_VALUE, -1L) must be 0 — the int WIDENS, so this"
                        + " reaches floorMod(JJ)J with an operand pair the (II)I row cannot"
                        + " produce");

        sectionEnd("divmod", 12);
    }

    // Ordered by how likely each family is to ABORT the VM rather than fail an
    // assertion, ascending. A Rust panic truncates the run, so anything after
    // the first aborting family never reports.
    static final String[] FAMILIES = {
        "charcls", "boolparse", "floatfmt", "hex", "b64", "uuid", "random", "strfmt",
        "bounds", "strictExact", "divmod",
    };

    static void runFamily(String name) {
        if ("charcls".equals(name)) {
            charcls();
        } else if ("boolparse".equals(name)) {
            boolparse();
        } else if ("floatfmt".equals(name)) {
            floatfmt();
        } else if ("hex".equals(name)) {
            hex();
        } else if ("b64".equals(name)) {
            b64();
        } else if ("uuid".equals(name)) {
            uuid();
        } else if ("random".equals(name)) {
            random();
        } else if ("strfmt".equals(name)) {
            strfmt();
        } else if ("bounds".equals(name)) {
            bounds();
        } else if ("strictExact".equals(name)) {
            strictExact();
        } else if ("divmod".equals(name)) {
            divmod();
        } else {
            throw new AssertionError("unknown family: " + name);
        }
    }

    public static void main(String[] args) {
        String only = null;
        for (int k = 0; k < args.length; k++) {
            if (args[k].startsWith("--only=")) {
                only = args[k].substring("--only=".length());
            } else if ("--list".equals(args[k])) {
                for (int j = 0; j < FAMILIES.length; j++) {
                    System.out.println("CK RJdkIntrinsics2 family=" + FAMILIES[j]);
                }
                return;
            }
        }
        if (only == null) {
            for (int k = 0; k < FAMILIES.length; k++) {
                runFamily(FAMILIES[k]);
            }
        } else {
            System.out.println("CK RJdkIntrinsics2 only=" + only);
            runFamily(only);
        }
        System.out.println("CK RJdkIntrinsics2 checks=" + checks);
        System.out.println("PASS RJdkIntrinsics2 (" + checks + " checks)");
    }
}
