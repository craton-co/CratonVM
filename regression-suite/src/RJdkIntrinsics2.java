import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.CharBuffer;
import java.nio.IntBuffer;
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
 * twelve families run in ascending order of how likely each is to ABORT the VM
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

    /**
     * Distance in ulps between two finite doubles, used to check {@code Math}'s
     * transcendentals against their bit-exact {@code StrictMath} twins.
     *
     * <p>{@code Math}'s javadoc gives those methods a ONE ULP error budget, so
     * a literal expected value would be over-strict and a plain {@code ==}
     * would be wrong. Comparing the ORDER of the two bit patterns is the exact
     * assertion the specification licenses: for two same-sign finite values the
     * signed-magnitude bit patterns are monotonic, so their difference counts
     * representable values between them. A NaN on either side is reported as an
     * infinite gap so it can never pass.
     */
    static double ulpGap(double a, double b) {
        if (Double.isNaN(a) || Double.isNaN(b)) {
            return Double.POSITIVE_INFINITY;
        }
        long x = Double.doubleToLongBits(a);
        long y = Double.doubleToLongBits(b);
        if ((x < 0) != (y < 0)) {
            // Opposite signs: only the two zeros may be this close.
            return a == b ? 0.0 : Double.POSITIVE_INFINITY;
        }
        long d = x - y;
        return d < 0 ? -(double) d : (double) d;
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
        // E26 additions — indices 27..32, APPENDED so every index above is
        // unchanged. 0x01c4 is the UPPERCASE Dz whose titlecase is 0x01c5.
        0x01c4, 0x4e00, 0x0378, 0x05d0, 0x0028, 0x0009,
    };
    /** Ordinary double operands: the MIDDLE of the domain, not its corners. */
    static final double[] OPAQUE_SM = {
        2.0, 0.5, 1.0, 3.0, 10.0, 123.456, 0.1, 1.5, 27.0, 1000.0, 4.0, 1e-10, 5.0,
        0.49999999999999994, -0.5, 22.0, 40.0, 2.5,
    };
    static final String[] OPAQUE_S = {
        "TRUE", "True", "tRuE", "1", "yes", "", " true", "z", "Z", "-z", "0", "+1",
        "--1", "-", "2147483648", "-2147483648", "१२", "ｆｆ",
        "9223372036854775808", "-9223372036854775808", "128", "-128", "7f", "80",
        "32768", "-32768", "ffff", "7fff", "nan", "inf", "0x1p3", "1.0f", "1.0d",
        "1e-46", "1e40", "1e-324", "1e400", "-0.0", "1.0dd", "  1.5  ",
        // E26 additions — indices 40..49, APPENDED so every index above is
        // unchanged.
        "4294967295", "ffffffff", "0x1f", "#1f", "017", "-0x1f", "08",
        "18446744073709551615", "hello123x", "4294967296",
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
            0x01c4, 0x4e00, 0x0378, 0x05d0, 0x0028, 0x0009,
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
                "Character.isISOControl(-1) must be false — the range test is SIGNED;"
                        + " widening the argument to unsigned still answers false, so a"
                        + " true here means the range itself is wrong");

        // forDigit is specified to return the NUL character for every input it
        // cannot map — never to throw and never to index out of a table.
        check(Character.forDigit(35, OPAQUE_I[10]) == 'z', "Character.forDigit(35, 36) must be 'z'");
        check(Character.forDigit(15, OPAQUE_I[11]) == 'f', "Character.forDigit(15, 16) must be 'f'");
        check(Character.forDigit(OPAQUE_I[12], OPAQUE_I[12]) == 0,
                "Character.forDigit(10, 10) must be U+0000 — digit >= radix");
        check(Character.forDigit(OPAQUE_I[2], OPAQUE_I[11]) == 0,
                "Character.forDigit(-1, 16) must be U+0000 — a negative digit is outside"
                        + " 0..radix; widening it to unsigned must not make it a digit");
        check(Character.forDigit(OPAQUE_I[3], OPAQUE_I[4]) == 0,
                "Character.forDigit(0, 1) must be U+0000 — radix below MIN_RADIX");
        check(Character.forDigit(OPAQUE_I[3], 37) == 0,
                "Character.forDigit(0, 37) must be U+0000 — radix above MAX_RADIX");

        // charCount / isValidCodePoint / isBmpCodePoint are arithmetic on an
        // int that is NOT required to be a valid code point.
        check(Character.charCount(OPAQUE_CP[9]) == 2, "Character.charCount(U+1F600) must be 2");
        check(Character.charCount(OPAQUE_CP[10]) == 1, "Character.charCount(U+FFFF) must be 1");
        check(Character.charCount(OPAQUE_I[2]) == 1,
                "Character.charCount(-1) must be 1 — the compare is SIGNED; widening the"
                        + " argument to unsigned answers 2");
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: eight classifiers, forDigit, digit, getNumericValue(C),
        // charCount, the four code-point/surrogate range tests and the two (I)I
        // case mappings. java.lang.Character declares ~60 public statics; the
        // rows above call 17 of them, and NONE of the three whole sub-surfaces
        // below.
        //
        // The block header already names the discriminating shape — "a body
        // that answers either predicate from char::is_alphabetic cannot get both
        // rows right" — and then never asks isAlphabetic. That is closed first.
        // ===================================================================

        // GAP 1: ONE code point, FOUR accessors, four answers. U+2160 is
        // isUpperCase=true and isLetter=false above; it is also
        // isAlphabetic=TRUE and isTitleCase=false, and its NUMERIC VALUE is 1
        // while Character.digit refuses it. Two methods that both "read a digit
        // out of a code point" must disagree here, which no single Rust call
        // produces.
        check(Character.isAlphabetic(OPAQUE_CP[2]),
                "Character.isAlphabetic(U+2160) must be TRUE — Nl is alphabetic, while"
                        + " isLetter on the SAME code point is false above");
        check(Character.isAlphabetic(OPAQUE_CP[19]),
                "Character.isAlphabetic(U+10400) must be true — astral Lu");
        check(!Character.isAlphabetic(OPAQUE_CP[4]),
                "Character.isAlphabetic(U+00B2) must be false — No is not alphabetic");
        check(!Character.isAlphabetic(OPAQUE_CP[9]),
                "Character.isAlphabetic(U+1F600) must be false");
        check(Character.getNumericValue(OPAQUE_CP[2]) == 1,
                "Character.getNumericValue(U+2160) must be 1 — ROMAN NUMERAL ONE is worth one");
        check(Character.digit(OPAQUE_CP[2], OPAQUE_I[10]) == -1,
                "Character.digit(U+2160, 36) must be -1 — the SAME code point the row above"
                        + " values at 1; digit() and getNumericValue() are NOT one function");
        check(Character.getNumericValue(OPAQUE_CP[24]) == 0,
                "Character.getNumericValue(int U+1D7CE) must be 0 — the (I)I overload, which is"
                        + " a different registered triple from the (C)I one above");
        check(Character.getNumericValue(OPAQUE_CP[11]) == -1,
                "Character.getNumericValue(int U+10000) must be -1 — no numeric value");
        check(Character.getNumericValue(OPAQUE_I[2]) == -1,
                "Character.getNumericValue(-1) must be -1, not a panic");
        check(!Character.isTitleCase(OPAQUE_CP[2]),
                "Character.isTitleCase(U+2160) must be false");
        check(Character.isTitleCase(OPAQUE_CP[1]),
                "Character.isTitleCase(U+01C5) must be TRUE — the one category the pair of"
                        + " upper/lower predicates above both answer false for");

        // GAP 2: toTitleCase — a THIRD case mapping, and the only one whose
        // fixed point is not its own input. Rust has no titlecase mapping at
        // all, so a body that aliases it onto to_uppercase gets U+01C4 wrong.
        check(Character.toTitleCase(OPAQUE_CP[27]) == 0x01c5,
                "Character.toTitleCase(U+01C4 UPPER DZ) must be U+01C5, NOT U+01C4 — the row"
                        + " that separates titlecase from uppercase");
        check(Character.toUpperCase(OPAQUE_CP[27]) == 0x01c4,
                "Character.toUpperCase(U+01C4) must be U+01C4 itself — the contrast");
        check(Character.toTitleCase(OPAQUE_CP[1]) == 0x01c5,
                "Character.toTitleCase(U+01C5) must be itself — already titlecase");
        check(Character.toTitleCase((int) 'a') == 'A',
                "Character.toTitleCase('a') must be 'A' where no distinct titlecase exists");
        check(Character.toTitleCase(OPAQUE_CP[18]) == 0x10400,
                "Character.toTitleCase(U+10428) must be U+10400 — astral");
        check(Character.toTitleCase(OPAQUE_I[2]) == -1,
                "Character.toTitleCase(-1) must be -1 — every unmapped int maps to itself");

        // GAP 3: getType. One method returning Unicode's general category,
        // never called, and it is the single answer from which most of the
        // predicates above are derived — so a VM can pass every predicate row
        // by hard-coding them and still have no category table.
        check(Character.getType(OPAQUE_CP[2]) == 10,
                "Character.getType(U+2160) must be 10 (LETTER_NUMBER)");
        check(Character.getType(OPAQUE_CP[1]) == 3,
                "Character.getType(U+01C5) must be 3 (TITLECASE_LETTER)");
        check(Character.getType(OPAQUE_CP[4]) == 11,
                "Character.getType(U+00B2) must be 11 (OTHER_NUMBER)");
        check(Character.getType(OPAQUE_CP[16]) == 19,
                "Character.getType(U+D800) must be 19 (SURROGATE) — a lone surrogate HAS a"
                        + " category; it is not unassigned");
        check(Character.getType(OPAQUE_CP[13]) == 0,
                "Character.getType(0x110000) must be 0 (UNASSIGNED), not a panic");
        check(Character.getType(OPAQUE_I[2]) == 0,
                "Character.getType(-1) must be 0 (UNASSIGNED)");
        check(Character.getType(OPAQUE_CP[9]) == 28,
                "Character.getType(U+1F600) must be 28 (OTHER_SYMBOL)");
        check(Character.getType(OPAQUE_CP[8]) == 12,
                "Character.getType(U+00A0) must be 12 (SPACE_SEPARATOR)");
        check(Character.getType(OPAQUE_CP[5]) == 15,
                "Character.getType(0x1C) must be 15 (CONTROL)");
        check(Character.getType(OPAQUE_CP[26]) == 9,
                "Character.getType(U+0F20) must be 9 (DECIMAL_DIGIT_NUMBER)");
        check(Character.LETTER_NUMBER == 10 && Character.TITLECASE_LETTER == 3
                        && Character.SURROGATE == 19 && Character.UNASSIGNED == 0,
                "the four category CONSTANTS the rows above name must hold their JLS values");

        // GAP 4: toChars / toCodePoint / isSurrogatePair — the surrogate
        // ARITHMETIC, as opposed to the surrogate range tests above. This is
        // where the family's contracts stop being uniform, deliberately:
        // charCount(0x110000) answers 2 without validating, and toChars(0x110000)
        // THROWS. Both rows are here so neither rule is generalised.
        check(Character.charCount(OPAQUE_CP[13]) == 2,
                "Character.charCount(0x110000) must be 2 — charCount NEVER validates");
        Throwable ct = null;
        try {
            sink = Character.toChars(OPAQUE_CP[13]).length;
        } catch (Throwable x) {
            ct = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ct)),
                "Character.toChars(0x110000) must THROW IllegalArgumentException — the same"
                        + " argument charCount answers 2 for; got " + nameOf(ct));
        ct = null;
        try {
            sink = Character.toChars(OPAQUE_I[2]).length;
        } catch (Throwable x) {
            ct = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ct)),
                "Character.toChars(-1) must throw IllegalArgumentException, got " + nameOf(ct));
        check(Character.toChars(OPAQUE_CP[9]).length == 2
                        && Character.toChars(OPAQUE_CP[9])[0] == 0xd83d
                        && Character.toChars(OPAQUE_CP[9])[1] == 0xde00,
                "Character.toChars(U+1F600) must be exactly { U+D83D, U+DE00 }");
        check(Character.toChars(OPAQUE_CP[10]).length == 1,
                "Character.toChars(U+FFFF) must be a ONE-element array");
        check(Character.toCodePoint((char) 0xd83d, (char) 0xde00) == 0x1f600,
                "Character.toCodePoint(D83D, DE00) must recombine to U+1F600");
        check(Character.isSurrogatePair((char) 0xd83d, (char) 0xde00),
                "Character.isSurrogatePair(high, low) must be true");
        check(!Character.isSurrogatePair((char) 0xde00, (char) 0xd83d),
                "Character.isSurrogatePair(low, high) must be FALSE — the ORDER matters");
        check(Character.isSurrogate((char) OPAQUE_CP[16]),
                "Character.isSurrogate(U+D800) must be true");
        check(!Character.isSurrogate('a'), "Character.isSurrogate('a') must be false");
        check(Character.isSupplementaryCodePoint(OPAQUE_CP[11]),
                "Character.isSupplementaryCodePoint(0x10000) must be true");
        check(!Character.isSupplementaryCodePoint(OPAQUE_CP[10]),
                "Character.isSupplementaryCodePoint(U+FFFF) must be false");
        check(!Character.isSupplementaryCodePoint(OPAQUE_CP[13]),
                "Character.isSupplementaryCodePoint(0x110000) must be FALSE — unlike charCount,"
                        + " this one DOES range-check the top end");
        check(Character.toString(OPAQUE_CP[9]).length() == 2,
                "Character.toString(int U+1F600) must be a TWO-char string — the (I) overload"
                        + " is a different triple from the (C) one above");
        ct = null;
        try {
            sink = Character.toString(OPAQUE_CP[13]).length();
        } catch (Throwable x) {
            ct = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ct)),
                "Character.toString(0x110000) must throw IllegalArgumentException, got "
                        + nameOf(ct));

        // GAP 5: Character's own CharSequence accessors — the statics that
        // shadow String's instance methods (which `bounds` exercises) and are
        // separate registered triples.
        String cpPair = "a" + new String(Character.toChars(OPAQUE_CP[9])) + "b";
        check(Character.codePointAt(cpPair, OPAQUE_I[4]) == 0x1f600,
                "Character.codePointAt(seq, 1) must PAIR the surrogates into U+1F600");
        check(Character.codePointAt(cpPair, OPAQUE_I[5]) == 0xde00,
                "Character.codePointAt(seq, 2) must be the LONE low surrogate — mid-pair, so"
                        + " there is nothing to pair it with");
        check(Character.codePointBefore(cpPair, OPAQUE_I[6]) == 0x1f600,
                "Character.codePointBefore(seq, 3) must walk BACK over the pair");
        check(Character.codePointCount(cpPair, OPAQUE_I[3], 4) == 3,
                "Character.codePointCount over \"a<U+1F600>b\" must be 3, not 4");
        check(Character.offsetByCodePoints(cpPair, OPAQUE_I[3], OPAQUE_I[5]) == 3,
                "Character.offsetByCodePoints(seq, 0, 2) must land at index 3");

        // GAP 6: space vs whitespace, and the identifier predicates. These are
        // the classic non-complementary pairs: TAB is whitespace and NOT a
        // space char; NBSP is a space char and NOT whitespace. A body that
        // answers both from Rust's `char::is_whitespace` gets exactly one of
        // each pair wrong.
        check(Character.isWhitespace(OPAQUE_CP[32]),
                "Character.isWhitespace(TAB) must be true");
        check(!Character.isSpaceChar(OPAQUE_CP[32]),
                "Character.isSpaceChar(TAB) must be FALSE — TAB is Cc, not Zs");
        check(Character.isSpaceChar(OPAQUE_CP[8]),
                "Character.isSpaceChar(U+00A0 NBSP) must be TRUE — it is Zs");
        check(!Character.isWhitespace(OPAQUE_CP[8]),
                "Character.isWhitespace(U+00A0) must be FALSE — Java excludes non-breaking"
                        + " spaces, and this is the exact inverse of the row above");
        check(Character.isSpaceChar(' '), "Character.isSpaceChar(' ') must be true");
        check(Character.isJavaIdentifierStart('$'),
                "Character.isJavaIdentifierStart('$') must be true");
        check(!Character.isUnicodeIdentifierStart('$'),
                "Character.isUnicodeIdentifierStart('$') must be FALSE — the Unicode grammar"
                        + " has no dollar sign; the two predicates differ on this one char");
        check(!Character.isJavaIdentifierPart(OPAQUE_CP[5]),
                "Character.isJavaIdentifierPart(0x1C) must be false");

        // GAP 7: the remaining property tables and the Object-ish statics.
        check(Character.isIdeographic(OPAQUE_CP[28]),
                "Character.isIdeographic(U+4E00) must be true");
        check(!Character.isIdeographic('a'), "Character.isIdeographic('a') must be false");
        check(!Character.isDefined(OPAQUE_CP[13]),
                "Character.isDefined(0x110000) must be false");
        check(!Character.isDefined(OPAQUE_CP[29]),
                "Character.isDefined(U+0378) must be false — an UNASSIGNED code point inside"
                        + " the BMP, which is the case a bare range check gets wrong");
        check(Character.isDefined(OPAQUE_CP[16]),
                "Character.isDefined(U+D800) must be TRUE — a surrogate IS assigned");
        check(Character.isMirrored((char) OPAQUE_CP[31]),
                "Character.isMirrored('(') must be true");
        check(!Character.isMirrored('a'), "Character.isMirrored('a') must be false");
        check(Character.getDirectionality('a') == 0,
                "Character.getDirectionality('a') must be 0 (LEFT_TO_RIGHT)");
        check(Character.getDirectionality(OPAQUE_CP[30]) == 1,
                "Character.getDirectionality(U+05D0 HEBREW ALEF) must be 1 (RIGHT_TO_LEFT)");
        check(Character.reverseBytes('A') == 0x4100,
                "Character.reverseBytes('A') must be U+4100");
        check(Character.compare('a', 'b') == -1,
                "Character.compare('a','b') must be -1 — the char DIFFERENCE, which is -1 here");
        check(Character.hashCode('a') == 97, "Character.hashCode('a') must be the code unit, 97");
        check(Character.valueOf('a') == Character.valueOf('a'),
                "Character.valueOf('a') must come from the JLS-mandated 0..127 box CACHE —"
                        + " IDENTITY, not equality");
        check(Character.isEmoji(OPAQUE_CP[9]), "Character.isEmoji(U+1F600) must be true");
        check(Character.isEmoji('#'),
                "Character.isEmoji('#') must be TRUE — NUMBER SIGN carries Emoji=Yes, which is"
                        + " the row a plausible 'is it a pictograph' body gets wrong");
        check(Character.isEmojiPresentation(OPAQUE_CP[9]),
                "Character.isEmojiPresentation(U+1F600) must be true");
        check(!Character.isEmojiPresentation(OPAQUE_CP[23]),
                "Character.isEmojiPresentation(U+2764) must be FALSE — it defaults to text");
        check(Character.isExtendedPictographic(OPAQUE_CP[23]),
                "Character.isExtendedPictographic(U+2764) must be true — the same code point"
                        + " the row above answers false for");
        check(Character.isEmojiComponent('#'), "Character.isEmojiComponent('#') must be true");

        sectionEnd("charcls", 167);
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: the SIGNED decimal/radix parsers, three radix
        // formatters (toString(x,radix), toHexString) and the Boolean family.
        // Never reached: the whole UNSIGNED half (parseUnsignedInt /
        // parseUnsignedLong / toUnsignedString), the OCTAL and BINARY
        // formatters, the `decode` grammar (three prefixes, none of which the
        // parse grammar accepts), the CharSequence-region parser, and the
        // boxing caches.
        //
        // VALUE-DOMAIN NOTE: the rows above drive the signed boundaries only.
        // Everything below is chosen where the SIGN INTERPRETATION of the same
        // 32 bits is what differs — which is precisely what a Rust body reaching
        // for i32 where Java means u32 (or the reverse) gets wrong, and it is
        // invisible to every row above.
        // ===================================================================

        // GAP 1: unsigned parse and unsigned format. Same bit pattern, two
        // readings, and the pair -1 <-> "4294967295" is the discriminator.
        check(Integer.parseUnsignedInt(OPAQUE_S[40]) == -1,
                "Integer.parseUnsignedInt(\"4294967295\") must be -1 — it fits u32 and it is"
                        + " the value parseInt REJECTS as an overflow");
        check(Integer.parseUnsignedInt(OPAQUE_S[41], OPAQUE_I[11]) == -1,
                "Integer.parseUnsignedInt(\"ffffffff\", 16) must be -1");
        check(Long.parseUnsignedLong(OPAQUE_S[47]) == -1L,
                "Long.parseUnsignedLong(\"18446744073709551615\") must be -1");
        check("4294967295".equals(Integer.toUnsignedString(OPAQUE_I[2])),
                "Integer.toUnsignedString(-1) must be \"4294967295\" — the inverse of the"
                        + " first row, and NOT \"-1\"");
        check("ffffffff".equals(Integer.toUnsignedString(OPAQUE_I[2], OPAQUE_I[11])),
                "Integer.toUnsignedString(-1, 16) must be \"ffffffff\"");
        check(Integer.toUnsignedString(OPAQUE_I[2], OPAQUE_I[5]).length() == 32,
                "Integer.toUnsignedString(-1, 2) must be 32 digits");
        check("18446744073709551615".equals(Long.toUnsignedString(OPAQUE_J[2])),
                "Long.toUnsignedString(-1L) must be \"18446744073709551615\"");
        check(Integer.toUnsignedLong(OPAQUE_I[2]) == 4294967295L,
                "Integer.toUnsignedLong(-1) must be 4294967295 — a ZERO-extending widen, where"
                        + " the i2l opcode sign-extends");
        check(Integer.compareUnsigned(OPAQUE_I[2], OPAQUE_I[4]) == 1,
                "Integer.compareUnsigned(-1, 1) must be 1 — signed compare says -1");
        check(Integer.compare(OPAQUE_I[2], OPAQUE_I[4]) == -1,
                "Integer.compare(-1, 1) must be -1 — the SIGNED twin, same operands");
        check(Byte.toUnsignedInt((byte) -1) == 255,
                "Byte.toUnsignedInt((byte) -1) must be 255");
        check(Short.toUnsignedInt((short) -1) == 65535,
                "Short.toUnsignedInt((short) -1) must be 65535");
        Throwable u = null;
        try {
            sink = Integer.parseUnsignedInt(OPAQUE_S[21]);
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(u)),
                "Integer.parseUnsignedInt(\"-128\") must throw — a MINUS SIGN is outside the"
                        + " unsigned grammar even though the magnitude fits; got " + nameOf(u));
        u = null;
        try {
            sink = Integer.parseUnsignedInt(OPAQUE_S[49]);
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(u)),
                "Integer.parseUnsignedInt(\"4294967296\") must throw — one past u32; got "
                        + nameOf(u));

        // GAP 2: the octal and binary formatters. Both are UNSIGNED like
        // toHexString, and both were never called.
        check("37777777777".equals(Integer.toOctalString(OPAQUE_I[2])),
                "Integer.toOctalString(-1) must be \"37777777777\" — unsigned");
        check(Integer.toBinaryString(OPAQUE_I[2]).length() == 32,
                "Integer.toBinaryString(-1) must be 32 digits");
        check("0".equals(Integer.toBinaryString(OPAQUE_I[3])),
                "Integer.toBinaryString(0) must be \"0\", not 32 zeros");
        check("101".equals(Integer.toBinaryString(5)),
                "Integer.toBinaryString(5) must be \"101\" — no leading zeros");
        check("1777777777777777777777".equals(Long.toOctalString(OPAQUE_J[2])),
                "Long.toOctalString(-1L) must be 22 sevens-and-a-one");
        check(Long.toBinaryString(Long.MIN_VALUE).length() == 64,
                "Long.toBinaryString(MIN_VALUE) must be 64 digits");
        check("ffffffffffffffff".equals(Long.toHexString(OPAQUE_J[2])),
                "Long.toHexString(-1L) must be sixteen f's");

        // GAP 3: `decode` — a THIRD grammar on the same classes, accepting
        // three prefixes that parseInt rejects and rejecting one thing it
        // accepts. "017" is the sharpest: parseInt reads 17, decode reads 15.
        check(Integer.decode(OPAQUE_S[42]).intValue() == 31,
                "Integer.decode(\"0x1f\") must be 31 — a prefix parseInt would reject");
        check(Integer.decode(OPAQUE_S[43]).intValue() == 31,
                "Integer.decode(\"#1f\") must be 31 — the '#' prefix is also hex");
        check(Integer.decode(OPAQUE_S[44]).intValue() == 15,
                "Integer.decode(\"017\") must be 15 — a LEADING ZERO means OCTAL, where"
                        + " Integer.parseInt(\"017\") is 17");
        check(Integer.parseInt(OPAQUE_S[44]) == 17,
                "Integer.parseInt(\"017\") must be 17 — the same string, the other grammar");
        check(Integer.decode(OPAQUE_S[45]).intValue() == -31,
                "Integer.decode(\"-0x1f\") must be -31 — the sign precedes the radix prefix");
        check(Integer.decode(OPAQUE_S[11]).intValue() == 1,
                "Integer.decode(\"+1\") must be 1");
        check(Long.decode(OPAQUE_S[43]).longValue() == 31L, "Long.decode(\"#1f\") must be 31");
        check(Byte.decode(OPAQUE_S[42]).byteValue() == 31,
                "Byte.decode(\"0x1f\") must be 31");
        check(Short.decode(OPAQUE_S[44]).shortValue() == 15, "Short.decode(\"017\") must be 15");
        u = null;
        try {
            sink = Integer.decode(OPAQUE_S[46]).intValue();
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(u)),
                "Integer.decode(\"08\") must throw — 8 is not an OCTAL digit; got " + nameOf(u));

        // GAP 4: THE NULL CONTRACTS ARE NOT UNIFORM, inside one class. Three
        // entry points, three different answers, and floatfmt below pins a
        // FOURTH for the same shape on Float. A body with one shared
        // null-guard cannot produce all four.
        u = null;
        try {
            sink = Integer.parseInt((String) null);
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(u)),
                "Integer.parseInt(null) must throw NumberFormatException — NOT NPE, which is"
                        + " what Float.valueOf(null) throws in floatfmt; got " + nameOf(u));
        u = null;
        try {
            sink = Integer.valueOf((String) null).intValue();
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(u)),
                "Integer.valueOf((String) null) must throw NumberFormatException, got "
                        + nameOf(u));
        u = null;
        try {
            sink = Integer.decode(null).intValue();
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(u)),
                "Integer.decode(null) must throw NullPointerException — the THIRD null contract"
                        + " on this class, and the odd one out; got " + nameOf(u));

        // GAP 5: valueOf(String), the region parser, and the boxing caches.
        check(Integer.valueOf(OPAQUE_S[7], OPAQUE_I[10]).intValue() == 35,
                "Integer.valueOf(\"z\", 36) must be 35 — a different triple from parseInt");
        check(Integer.parseInt(OPAQUE_S[48], 5, 8, OPAQUE_I[12]) == 123,
                "Integer.parseInt(\"hello123x\", 5, 8, 10) must be 123 — the CharSequence"
                        + " REGION overload, which never allocates a substring");
        u = null;
        try {
            sink = Integer.parseInt(OPAQUE_S[48], OPAQUE_I[3], 99, OPAQUE_I[12]);
        } catch (Throwable x) {
            u = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(u)),
                "Integer.parseInt(seq, 0, 99, 10) must throw IndexOutOfBoundsException — a bad"
                        + " REGION is not a bad NUMBER, so it is not NumberFormatException; got "
                        + nameOf(u));
        check(Integer.valueOf(127) == Integer.valueOf(127),
                "Integer.valueOf(127) must come from the JLS-mandated -128..127 cache —"
                        + " IDENTITY, not equality");
        check(Long.valueOf(-128L) == Long.valueOf(-128L),
                "Long.valueOf(-128L) must come from the cache too");
        check(Integer.hashCode(42) == 42, "Integer.hashCode(42) must be the value itself");
        check(Long.hashCode(OPAQUE_J[2]) == 0,
                "Long.hashCode(-1L) must be 0 — (int)(v ^ (v >>> 32)) folds -1 to zero, which"
                        + " no identity-shaped hash produces");
        check(Long.hashCode(4294967296L) == 1, "Long.hashCode(2^32) must be 1");

        // GAP 6: the bit-level siblings on the same two classes. The rotates
        // are the hazard: Java MASKS the distance to 5 (or 6) bits, so a
        // negative or over-wide distance is well defined, where Rust's shift
        // operators panic and its rotate_left takes an unsigned distance.
        check(Integer.rotateLeft(OPAQUE_I[4], OPAQUE_I[2]) == Integer.MIN_VALUE,
                "Integer.rotateLeft(1, -1) must be MIN_VALUE — the distance is masked to 31,"
                        + " never rejected");
        check(Integer.rotateLeft(OPAQUE_I[4], 32) == 1,
                "Integer.rotateLeft(1, 32) must be 1 — a full turn, not a shift-overflow");
        check(Integer.rotateRight(OPAQUE_I[4], OPAQUE_I[2]) == 2,
                "Integer.rotateRight(1, -1) must be 2");
        check(Long.rotateLeft(OPAQUE_J[4], OPAQUE_I[2]) == Long.MIN_VALUE,
                "Long.rotateLeft(1L, -1) must be MIN_VALUE — masked to 63");
        check(Integer.reverse(OPAQUE_I[4]) == Integer.MIN_VALUE,
                "Integer.reverse(1) must be MIN_VALUE");
        check(Integer.reverseBytes(OPAQUE_I[4]) == 16777216,
                "Integer.reverseBytes(1) must be 0x01000000");
        check(Integer.highestOneBit(OPAQUE_I[3]) == 0,
                "Integer.highestOneBit(0) must be 0, not a panic on a leading-zero count of 32");
        check(Integer.highestOneBit(OPAQUE_I[2]) == Integer.MIN_VALUE,
                "Integer.highestOneBit(-1) must be MIN_VALUE — the SIGN bit is a bit");
        check(Integer.lowestOneBit(OPAQUE_I[3]) == 0, "Integer.lowestOneBit(0) must be 0");
        check(Integer.numberOfLeadingZeros(OPAQUE_I[3]) == 32,
                "Integer.numberOfLeadingZeros(0) must be 32");
        check(Integer.numberOfTrailingZeros(OPAQUE_I[3]) == 32,
                "Integer.numberOfTrailingZeros(0) must be 32");
        check(Integer.bitCount(OPAQUE_I[2]) == 32, "Integer.bitCount(-1) must be 32");
        check(Long.bitCount(OPAQUE_J[2]) == 64, "Long.bitCount(-1L) must be 64");
        check(Long.numberOfTrailingZeros(OPAQUE_J[3]) == 64,
                "Long.numberOfTrailingZeros(0L) must be 64");
        check(Integer.signum(Integer.MIN_VALUE) == -1,
                "Integer.signum(MIN_VALUE) must be -1 — computed without negating");
        check(!Boolean.logicalXor(true, true), "Boolean.logicalXor(true, true) must be false");

        sectionEnd("boolparse", 115);
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: toString, the four bit accessors, compare/hashCode/
        // equals, valueOf(String)/parseX. Never reached: toHexString (an
        // entirely SEPARATE formatter with its own grammar), the max/min/sum
        // statics, isFinite, and every narrowing conversion off a box.
        //
        // VALUE-DOMAIN NOTE — this is the block the task's own thesis is about.
        // Of the 47 rows above, FORTY-ONE drive a corner: NaN, +-0.0,
        // +-Infinity, MAX_VALUE, MIN_VALUE, MIN_NORMAL, a subnormal, an
        // underflow, an overflow. The only ordinary inputs anywhere are 0.1f,
        // 1.1f, 1e7f, 1e-3f and 1e20f. A shortest-round-trip printer or a
        // decimal->binary parser that is wrong in the MIDDLE — the 44-ulp shape
        // — passes every one of those 41 rows. Everything under GAP 2 is
        // ordinary.
        // ===================================================================

        // GAP 1: toHexString. Java's hex-float grammar is not Rust's `{:a}`
        // (which does not exist) and not printf's %a either at the subnormal
        // end: a SUBNORMAL prints with a LEADING ZERO and the fixed exponent
        // p-1022, where a normal value prints "0x1." and its own exponent.
        check("0x1.0p0".equals(Double.toHexString(OPAQUE_SM[2])),
                "Double.toHexString(1.0) must be \"0x1.0p0\"");
        check("0x1.0p-1".equals(Double.toHexString(OPAQUE_D[6])),
                "Double.toHexString(0.5) must be \"0x1.0p-1\"");
        check("-0x0.0p0".equals(Double.toHexString(OPAQUE_D[0])),
                "Double.toHexString(-0.0) must keep the sign: \"-0x0.0p0\"");
        check("0x0.0000000000001p-1022".equals(Double.toHexString(Double.MIN_VALUE)),
                "Double.toHexString(MIN_VALUE) must be the SUBNORMAL form — leading \"0x0.\""
                        + " and the exponent pinned at p-1022, not p-1074");
        check("0x1.0p-1022".equals(Double.toHexString(Double.MIN_NORMAL)),
                "Double.toHexString(MIN_NORMAL) must be \"0x1.0p-1022\" — one ulp of exponent"
                        + " from the row above and a completely different shape");
        check("0x1.fffffffffffffp1023".equals(Double.toHexString(Double.MAX_VALUE)),
                "Double.toHexString(MAX_VALUE) must be \"0x1.fffffffffffffp1023\"");
        check("NaN".equals(Double.toHexString(OPAQUE_D[2])),
                "Double.toHexString(NaN) must be \"NaN\", not a hex pattern");
        check("0x1.999999999999ap-4".equals(Double.toHexString(OPAQUE_SM[6])),
                "Double.toHexString(0.1) must expose the FULL 52-bit significand");
        check("0x1.0p0".equals(Float.toHexString(1.0f)), "Float.toHexString(1.0f) must be \"0x1.0p0\"");
        check("0x0.000002p-126".equals(Float.toHexString(OPAQUE_F[4])),
                "Float.toHexString(Float.MIN_VALUE) must be \"0x0.000002p-126\" — six hex"
                        + " digits and the FLOAT subnormal exponent, not the double's");

        // GAP 2: THE ORDINARY MIDDLE. Every value here is a plain decimal that
        // no boundary test reaches, and each row is a place where a
        // shortest-round-trip printer that is merely "close" prints a
        // different string.
        check("0.30000000000000004".equals(Double.toString(0.1 + 0.2)),
                "Double.toString(0.1 + 0.2) must be \"0.30000000000000004\" — SEVENTEEN"
                        + " significant digits, because sixteen do not round-trip");
        check("0.3333333333333333".equals(Double.toString(OPAQUE_SM[2] / OPAQUE_SM[3])),
                "Double.toString(1.0/3) must be \"0.3333333333333333\" — SIXTEEN digits, one"
                        + " fewer than the row above, and a printer with a fixed digit count"
                        + " gets exactly one of these two rows right");
        check("0.6666666666666666".equals(Double.toString(OPAQUE_SM[0] / OPAQUE_SM[3])),
                "Double.toString(2.0/3) must be \"0.6666666666666666\" — NOT ...67, the"
                        + " shortest string that round-trips is the truncated one");
        check("1.0E23".equals(Double.toString(1e23)),
                "Double.toString(1e23) must be \"1.0E23\" — the classic shortest-repr case"
                        + " where the nearest double is 9.999999999999999E22");
        check("1.234567890123E9".equals(Double.toString(1234567890.123)),
                "Double.toString(1234567890.123) must switch to scientific at 1e7");
        check("1.7976931348623157E308".equals(Double.toString(Double.MAX_VALUE)),
                "Double.toString(MAX_VALUE) must be \"1.7976931348623157E308\"");
        check("2.2250738585072014E-308".equals(Double.toString(Double.MIN_NORMAL)),
                "Double.toString(MIN_NORMAL) must be \"2.2250738585072014E-308\"");
        check("0.33333334".equals(Float.toString(1.0f / 3.0f)),
                "Float.toString(1.0f/3) must be \"0.33333334\" — EIGHT digits");
        check("1.6777216E7".equals(Float.toString(16777216f)),
                "Float.toString(2^24) must be \"1.6777216E7\"");
        check(Double.doubleToRawLongBits((double) 0.1f) == 0x3fb99999a0000000L,
                "the f2d widening of 0.1f must be 0.10000000149011612, NOT 0.1 — a body that"
                        + " round-trips through a decimal string collapses these two");
        check(Double.doubleToRawLongBits(Double.parseDouble("0.1")) == 0x3fb999999999999aL,
                "Double.parseDouble(\"0.1\") must be the CORRECTLY ROUNDED double, ...99a");
        check(Float.floatToRawIntBits(Float.parseFloat("0.1")) == 0x3dcccccd,
                "Float.parseFloat(\"0.1\") must round in FLOAT precision, not parse as a double"
                        + " and narrow — the two agree here and the next row is where they part");
        check(Double.doubleToRawLongBits(Double.parseDouble("2.2250738585072012e-308"))
                        == 0x10000000000000L,
                "Double.parseDouble of the 2.2250738585072012e-308 half-way subnormal must be"
                        + " MIN_NORMAL — the input that used to hang naive decimal->binary loops");
        check(Double.doubleToRawLongBits(Double.parseDouble("0.30000000000000004"))
                        == Double.doubleToRawLongBits(0.1 + 0.2),
                "parsing the 17-digit string back must reproduce 0.1 + 0.2 exactly");
        // A round-trip SWEEP over ordinary values. One row, twelve operands:
        // a printer that is wrong anywhere in the middle fails here even if
        // every hand-picked string above happens to match.
        double[] rt = {
            OPAQUE_SM[6], 0.2, 0.3, OPAQUE_SM[2] / OPAQUE_SM[3], OPAQUE_D[4], StrictMath.E,
            OPAQUE_SM[11], 1.5e300, 6.02214076e23, 4.35, 1234567890.123, 0.1 + 0.2,
        };
        boolean allRoundTrip = true;
        for (int k = 0; k < rt.length; k++) {
            if (Double.doubleToRawLongBits(Double.parseDouble(Double.toString(rt[k])))
                    != Double.doubleToRawLongBits(rt[k])) {
                allRoundTrip = false;
            }
        }
        check(allRoundTrip,
                "every one of the twelve ORDINARY doubles must survive"
                        + " toString -> parseDouble with identical bits");

        // GAP 3: max/min/sum and the narrowing conversions off a box. -0.0 is
        // the operand on which max/min are NOT the arithmetic comparison.
        check(Double.doubleToRawLongBits(Math.max(OPAQUE_D[0], OPAQUE_D[1])) == 0L,
                "Math.max(-0.0, 0.0) must be POSITIVE zero — == cannot tell them apart");
        check(Double.doubleToRawLongBits(Math.min(OPAQUE_D[0], OPAQUE_D[1])) == Long.MIN_VALUE,
                "Math.min(-0.0, 0.0) must be NEGATIVE zero");
        check(Double.doubleToRawLongBits(Math.max(OPAQUE_D[2], OPAQUE_SM[2]))
                        == 0x7ff8000000000000L,
                "Math.max(NaN, 1.0) must be NaN — NaN POISONS max, it does not lose to it");
        check(Float.floatToRawIntBits(Float.max(OPAQUE_F[0], OPAQUE_F[1])) == 0,
                "Float.max(-0.0f, 0.0f) must be positive zero");
        check(Float.floatToRawIntBits(Float.sum(OPAQUE_F[0], OPAQUE_F[0])) == 0x80000000,
                "Float.sum(-0.0f, -0.0f) must be NEGATIVE zero — the only sum of two zeros"
                        + " that is not positive");
        check(Double.doubleToRawLongBits(Double.sum(OPAQUE_D[0], OPAQUE_D[1])) == 0L,
                "Double.sum(-0.0, 0.0) must be positive zero");
        check(Double.isFinite(Double.MAX_VALUE), "Double.isFinite(MAX_VALUE) must be true");
        check(!Double.isFinite(OPAQUE_D[2]), "Double.isFinite(NaN) must be false");
        check((int) 1e20f == Integer.MAX_VALUE,
                "the f2i conversion of 1e20f must SATURATE to Integer.MAX_VALUE");
        check((int) OPAQUE_F[2] == 0, "the f2i conversion of NaN must be 0");
        check((long) Double.NEGATIVE_INFINITY == Long.MIN_VALUE,
                "the d2l conversion of -Infinity must saturate to Long.MIN_VALUE");
        check(Float.valueOf(OPAQUE_F[2]).intValue() == 0,
                "Float.valueOf(NaN).intValue() must be 0");
        check(Double.valueOf(1.9).intValue() == 1,
                "Double.valueOf(1.9).intValue() must TRUNCATE to 1, not round to 2");
        check(Double.valueOf(-1.9).longValue() == -1L,
                "Double.valueOf(-1.9).longValue() must truncate TOWARD ZERO to -1, not to -2");
        check(!Double.valueOf(OPAQUE_SM[2]).equals(Float.valueOf(1.0f)),
                "Double.valueOf(1.0).equals(Float.valueOf(1.0f)) must be FALSE — equals is"
                        + " type-exact even when the values agree");
        check(Double.valueOf(OPAQUE_SM[2]).compareTo(Double.valueOf(OPAQUE_SM[0])) == -1,
                "Double.valueOf(1.0).compareTo(2.0) must be -1");
        check(Float.hashCode(OPAQUE_F[4]) == 1,
                "Float.hashCode(Float.MIN_VALUE) must be 1 — the raw bits of the smallest"
                        + " subnormal");

        sectionEnd("floatfmt", 89);
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

        // ===================================================================
        // E26 — the reach audit. This is the largest single hole in the file.
        //
        // REACH BEFORE: HexFormat is a FORMATTER AND A PARSER, and the 26 rows
        // above call the formatter only. Not one of parseHex (three overloads),
        // fromHexDigits (two), fromHexDigitsToLong, fromHexDigit, isHexDigit,
        // toLowHexDigit, toHighHexDigit, toHexDigits(char/short), the
        // digit-count toHexDigits(long,int), formatHex(Appendable,..),
        // toString, equals or hashCode is reached. Sixteen registered triples,
        // and the fixture exercises the half that cannot fail on a bad input.
        //
        // OBJECT-STATE NOTE: every parse row below is driven through BOTH a
        // default and a CONFIGURED formatter, because a parser that ignores its
        // own delimiter/prefix/suffix state is exactly the "fixture only ever
        // used the default instance" shape.
        //
        // TRAP THIS BLOCK IS BUILT AROUND: the failure contract is NOT uniform.
        // An odd length is IllegalArgumentException, a bad DIGIT is
        // NumberFormatException, a bad RANGE is IndexOutOfBoundsException and a
        // null is NullPointerException — four classes from one method family.
        // Do not read one row as the rule for the others.
        // ===================================================================

        check(Arrays.equals(f.parseHex("00ff0a80"), hb),
                "HexFormat.of().parseHex must invert its own formatHex");
        check(Arrays.equals(f.parseHex("00FF0A80"), hb),
                "parseHex must accept UPPERCASE input on a LOWERCASE formatter — the case"
                        + " setting governs OUTPUT only");
        check(Arrays.equals(f.withUpperCase().parseHex("00ff"), new byte[] { 0, (byte) 0xff }),
                "and the reverse: an uppercase formatter must accept lowercase input");
        check(f.parseHex("").length == 0, "parseHex(\"\") must be an empty array");
        check(Arrays.equals(f.parseHex("00ff0a80", 2, 6), new byte[] { (byte) 0xff, 0x0a }),
                "parseHex(seq, 2, 6) must be the half-open CHARACTER range, two bytes");
        check(Arrays.equals(f.parseHex("x00ff0a80".toCharArray(), 1, 5),
                        new byte[] { 0, (byte) 0xff }),
                "the char[] overload's (fromIndex, toIndex) must be a half-open RANGE — the"
                        + " class javadoc calls them offset/length (HexFormat.java:79) and the"
                        + " SIGNATURE (:577) does not. Read as offset/length, (1, 5) is the"
                        + " five characters \"00ff0\", an ODD length that throws; read as a"
                        + " range it is \"00ff\" = {0, 0xff}. Both measured on jdk-25.0.3+9."
                        + " F2-1 NOMINATION 3");
        check(Arrays.equals(HexFormat.ofDelimiter(":").parseHex("00:ff:0a:80"), hb),
                "a CONFIGURED formatter's parseHex must consume its own delimiter");
        check(Arrays.equals(HexFormat.ofDelimiter(", ").withPrefix("0x").withSuffix("!")
                        .parseHex("0x00!, 0xff!, 0x0a!, 0x80!"), hb),
                "prefix + suffix + delimiter must all be stripped on the way back in — this is"
                        + " the round trip of the 'prefix and suffix wrap each byte' row above");

        // The four DIFFERENT failure classes, asserted separately.
        Throwable h = null;
        try {
            sink = f.parseHex("0f0").length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(h)),
                "parseHex(\"0f0\") must throw IllegalArgumentException — an ODD length is a"
                        + " structural error; got " + nameOf(h));
        h = null;
        try {
            sink = f.parseHex("zz").length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(h)),
                "parseHex(\"zz\") must throw NumberFormatException — a bad DIGIT is a different"
                        + " class from a bad LENGTH, one row up; got " + nameOf(h));
        h = null;
        try {
            sink = f.parseHex("00ff", 0, 9).length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(h)),
                "parseHex(seq, 0, 9) must throw IndexOutOfBoundsException — the THIRD class;"
                        + " got " + nameOf(h));
        h = null;
        try {
            sink = f.parseHex((CharSequence) null).length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(h)),
                "parseHex(null) must throw NullPointerException — the FOURTH; got " + nameOf(h));
        h = null;
        try {
            sink = HexFormat.ofDelimiter(":").parseHex("00ff").length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(h)),
                "a delimited formatter must REJECT undelimited input — the state has to be"
                        + " read on the way in, not only on the way out; got " + nameOf(h));
        h = null;
        try {
            sink = f.parseHex("0x00!, 0xff!").length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(h)),
                "the DEFAULT formatter must reject prefixed text the configured one accepts;"
                        + " got " + nameOf(h));

        // fromHexDigits — an UNSIGNED accumulator with a silent-truncation
        // contract that the formatter half cannot expose.
        check(HexFormat.fromHexDigits("ff") == 255, "fromHexDigits(\"ff\") must be 255");
        check(HexFormat.fromHexDigits("FF") == 255, "fromHexDigits(\"FF\") must be 255");
        check(HexFormat.fromHexDigits("ffffffff") == -1,
                "fromHexDigits(\"ffffffff\") must be -1 — it fills the int and WRAPS to"
                        + " negative rather than overflowing");
        check(HexFormat.fromHexDigits("") == 0, "fromHexDigits(\"\") must be 0, not a throw");
        check(HexFormat.fromHexDigits("abcdef", 2, 4) == 205,
                "fromHexDigits(seq, 2, 4) must read only \"cd\"");
        check(HexFormat.fromHexDigitsToLong("ffffffffffffffff") == -1L,
                "fromHexDigitsToLong of sixteen f's must be -1");
        check(HexFormat.fromHexDigitsToLong("7fffffffffffffff") == Long.MAX_VALUE,
                "fromHexDigitsToLong(\"7fff...\") must be Long.MAX_VALUE");
        h = null;
        try {
            sink = HexFormat.fromHexDigits("1ffffffff");
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(h)),
                "fromHexDigits with NINE digits must throw IllegalArgumentException — more"
                        + " than eight is rejected, but exactly eight silently wraps two rows"
                        + " above; got " + nameOf(h));
        h = null;
        try {
            sink = HexFormat.fromHexDigits(OPAQUE_S[42]);
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(h)),
                "fromHexDigits(\"0x1f\") must throw NumberFormatException — there is no radix"
                        + " PREFIX in this grammar, unlike Integer.decode; got " + nameOf(h));

        // The single-digit helpers, including the two that are NOT symmetric.
        check(HexFormat.fromHexDigit('a') == 10, "fromHexDigit('a') must be 10");
        check(HexFormat.fromHexDigit('F') == 15, "fromHexDigit('F') must be 15");
        h = null;
        try {
            sink = HexFormat.fromHexDigit('g');
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(h)),
                "fromHexDigit('g') must throw NumberFormatException, got " + nameOf(h));
        h = null;
        try {
            sink = HexFormat.fromHexDigit(OPAQUE_I[2]);
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(h)),
                "fromHexDigit(-1) must THROW where isHexDigit(-1) merely answers false — the"
                        + " test and the converter do not share a contract; got " + nameOf(h));
        check(HexFormat.isHexDigit('a') && HexFormat.isHexDigit('F') && HexFormat.isHexDigit('0'),
                "isHexDigit must accept both cases and the decimal digits");
        check(!HexFormat.isHexDigit('g'), "isHexDigit('g') must be false");
        check(!HexFormat.isHexDigit(OPAQUE_I[2]),
                "isHexDigit(-1) must be FALSE, not a panic and not a throw");
        check(!HexFormat.isHexDigit(0x10030),
                "isHexDigit(U+10030) must be false — an ASTRAL code point is not a hex digit"
                        + " even though its low byte is '0'");
        check(f.toLowHexDigit(OPAQUE_I[7]) == 'f',
                "toLowHexDigit(255) must be 'f' — the LOW nibble");
        check(f.toHighHexDigit(OPAQUE_I[7]) == 'f', "toHighHexDigit(255) must be 'f'");
        check(f.toHighHexDigit(0x1a) == '1',
                "toHighHexDigit(0x1A) must be '1' — the HIGH nibble, where toLowHexDigit is 'a'");
        check(f.toLowHexDigit(0x1a) == 'a', "toLowHexDigit(0x1A) must be 'a'");
        check(f.withUpperCase().toLowHexDigit(OPAQUE_I[7]) == 'F',
                "the case setting must reach the single-digit helpers too");

        // The remaining formatters and the Object contract.
        check("0041".equals(f.toHexDigits('A')),
                "toHexDigits(char 'A') must be FOUR digits, \"0041\"");
        check("ffff".equals(f.toHexDigits((short) -1)),
                "toHexDigits(short -1) must be \"ffff\" — four digits, unsigned");
        check("fff".equals(f.toHexDigits(OPAQUE_J[2], 3)),
                "toHexDigits(-1L, 3) must be the LOW three digits");
        check("".equals(f.toHexDigits(OPAQUE_J[2], 0)),
                "toHexDigits(-1L, 0) must be the empty string, not a throw");
        h = null;
        try {
            sink = f.toHexDigits(OPAQUE_J[2], 17).length();
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(h)),
                "toHexDigits(-1L, 17) must throw IllegalArgumentException — 16 is the maximum;"
                        + " got " + nameOf(h));
        StringBuilder hsb = new StringBuilder("Z");
        f.formatHex(hsb, hb);
        check("Z00ff0a80".equals(hsb.toString()),
                "formatHex(Appendable, hb) must APPEND, not replace");
        check("uppercase: false, delimiter: \"\", prefix: \"\", suffix: \"\"".equals(f.toString()),
                "HexFormat.of().toString() must report all four settings");
        check(f.equals(HexFormat.of()), "two HexFormat.of() must be equal");
        check(f.hashCode() == HexFormat.of().hashCode(), "and their hashCodes must agree");
        check(!f.equals(HexFormat.ofDelimiter(":")),
                "a delimited formatter must NOT equal the default one");
        check(f == HexFormat.of(),
                "HexFormat.of() must be a SINGLETON — measured identity on HotSpot 25; a VM"
                        + " that fabricates a fresh receiver per factory call fails here and"
                        + " nowhere else, which is exactly how the Base64 factories failed");

        // F2-1 NOMINATION 3, second and third halves. APPENDED at the end of
        // the family ON PURPOSE: F2's prediction table is keyed by check number
        // (it stops at 32 of 73 and names 32-49, 54-57, 69, 73), and inserting
        // these beside the other parseHex rows would have renumbered every one
        // of them. These are checks 74-77; every number in that table still
        // means what it meant. The denominator moves 73 -> 77.
        //
        // The DEFECT F2-1 §1.1 found has no row anywhere in this file: every
        // parseHex row above passes a `String`, and the native's reader is
        // class-guarded to real java.lang.String, so a non-String CharSequence
        // read back EMPTY and parseHex answered a zero-length array. Silent —
        // no exception, no wrong byte, just nothing. The JDK's own ranged
        // bytecode manufactures exactly such an operand (HexFormat.java:577-582
        // wraps the char[] in a CharBuffer and calls the one-argument form), so
        // the whole ranged family rode on a reader no row ever exercised.
        check(Arrays.equals(f.parseHex(new StringBuilder("00ff0a80")), hb),
                "parseHex(CharSequence) must read a NON-String CharSequence — the parameter"
                        + " is CharSequence, and a reader that only understands java.lang.String"
                        + " answers an EMPTY array here rather than throwing; measured [0, -1,"
                        + " 10, -128] on jdk-25.0.3+9");
        check(Arrays.equals(f.parseHex(CharBuffer.wrap("x00ff0a80y", 1, 9)), hb),
                "and a CharBuffer specifically — this is the operand the JDK's own"
                        + " parseHex(char[], int, int) builds internally, so this row is what"
                        + " stands between the ranged overloads and a silent empty answer;"
                        + " its length() is the REMAINING count, 8");
        h = null;
        try {
            sink = f.parseHex("x00ff0a80".toCharArray(), OPAQUE_I[6], OPAQUE_I[4]).length;
        } catch (Throwable x) {
            h = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(h)),
                "parseHex(char[], 3, 1) — a REVERSED range — must throw"
                        + " IndexOutOfBoundsException, not return an empty array and not"
                        + " underflow a length computation; got " + nameOf(h));
        check("Range [3, 1) out of bounds for length 9".equals(h.getMessage()),
                "and the message counts the WHOLE operand (length 9), not the slice — where"
                        + " the odd-length IllegalArgumentException one family up counts the"
                        + " SLICE (\"string length not even: 3\"). Two bounds messages, two"
                        + " different denominators; got " + h.getMessage());

        sectionEnd("hex", 77);
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

        // E14: the surface the 27 rows above never reach — identity, a custom
        // linemax, and the methods that run real JDK bytecode against a
        // receiver this VM fabricates. Every expected value measured on
        // HotSpot 25 (scratchpad/e14).
        check(Base64.getEncoder() == Base64.getEncoder(),
                "the factories are SINGLETONS — identity, not equality");
        check(Base64.getMimeEncoder(0, new byte[] { '\n' }) == Base64.getEncoder(),
                "getMimeEncoder(lineLength<=0) must return the basic encoder ITSELF");
        check(Base64.getMimeEncoder(20, new byte[] { '\n' }).encodeToString(big).length() == 83,
                "a custom linemax must be honoured: 80 chars + 3 one-byte separators");
        byte[] dst = new byte[200];
        check(Base64.getEncoder().encode(big, dst) == 80,
                "encode(byte[],byte[]) must write 80 bytes for the basic encoder");
        check(Base64.getMimeEncoder().encode(big, dst) == 82,
                "encode(byte[],byte[]) must write 82 for the MIME encoder — it reads `newline`");
        check(Base64.getMimeDecoder().decode(
                        mime.getBytes(java.nio.charset.StandardCharsets.ISO_8859_1), dst) == 60,
                "decode(byte[],byte[]) on the MIME decoder must accept the wrapped text");
        java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
        try (java.io.OutputStream os = Base64.getEncoder().wrap(bo)) {
            os.write(big);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        check(bo.size() == 80, "getEncoder().wrap(OutputStream) must write 80 bytes");

        // ===================================================================
        // E26 — the reach audit, on top of E14's N1.
        //
        // REACH AFTER N1: the String and byte[] encode/decode paths, identity,
        // one custom linemax, wrap(OutputStream). Still unreached: the
        // ByteBuffer overloads (the only ones with POSITION state),
        // wrap(InputStream), the getMimeEncoder(int,byte[]) argument
        // validation, and the decoder VARIANT MATRIX.
        //
        // TRAP THIS BLOCK IS BUILT AROUND, measured here, not remembered: the
        // three decoders do NOT agree, and "-_-_" has THREE different correct
        // answers — basic throws, MIME returns an EMPTY array, URL decodes it.
        // A row that asserts one of those for the wrong decoder is worse than
        // no row.
        // ===================================================================

        // The decoder matrix. Every cell measured on HotSpot 25.
        check(Arrays.equals(Base64.getUrlDecoder().decode("-_-_"), new byte[] { -5, -1, -65 }),
                "URL decoder on \"-_-_\" -> 3 bytes");
        check(Base64.getMimeDecoder().decode("-_-_").length == 0,
                "MIME decoder on \"-_-_\" -> an EMPTY array: it SKIPS both illegal characters"
                        + " and is then left with nothing, where the basic decoder THROWS on the"
                        + " same input and the URL decoder returns three bytes");
        check(Arrays.equals(Base64.getMimeDecoder().decode("Q*Q=="), new byte[] { 65 }),
                "MIME decoder skips an illegal character mid-group");
        check(Arrays.equals(Base64.getMimeDecoder().decode("+/+/"), new byte[] { -5, -1, -65 }),
                "the MIME decoder uses the BASIC alphabet, so '+' and '/' are legal to it");
        String[] allThreeReject = { "QQ=", "A", "QQ==X" };
        for (int k = 0; k < allThreeReject.length; k++) {
            Throwable bt = null;
            try {
                sink = Base64.getMimeDecoder().decode(allThreeReject[k]).length;
            } catch (Throwable x) {
                bt = x;
            }
            check("java.lang.IllegalArgumentException".equals(nameOf(bt)),
                    "even the LENIENT MIME decoder must reject \"" + allThreeReject[k]
                            + "\" — skipping illegal characters is not the same as accepting a"
                            + " broken padding group; got " + nameOf(bt));
        }
        Throwable ub = null;
        try {
            sink = Base64.getUrlDecoder().decode("+/+/").length;
        } catch (Throwable x) {
            ub = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ub)),
                "the URL decoder must REJECT the basic alphabet — the mirror of the row the"
                        + " block above asserts for the basic decoder; got " + nameOf(ub));

        // The ByteBuffer overloads: the only Base64 entry points with STATE.
        // Both must consume their source, which a body that reads from index 0
        // and never advances the position gets wrong while still producing the
        // right bytes.
        java.nio.ByteBuffer bsrc = java.nio.ByteBuffer.wrap(bb);
        java.nio.ByteBuffer bout = Base64.getEncoder().encode(bsrc);
        check(bsrc.position() == 5,
                "encode(ByteBuffer) must ADVANCE the source position to its limit");
        check(bout.remaining() == 8, "encode(ByteBuffer) must return 8 remaining bytes");
        java.nio.ByteBuffer dsrc = java.nio.ByteBuffer.wrap(
                "+/+/AAE=".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1));
        java.nio.ByteBuffer dbout = Base64.getDecoder().decode(dsrc);
        check(dsrc.position() == 8, "decode(ByteBuffer) must advance the source position to 8");
        check(dbout.remaining() == 5, "decode(ByteBuffer) must yield the 5 original bytes");
        java.io.InputStream dis = Base64.getDecoder().wrap(
                new java.io.ByteArrayInputStream(
                        "+/+/AAE=".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1)));
        byte[] streamed;
        try {
            streamed = dis.readAllBytes();
            dis.close();
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        check(Arrays.equals(streamed, bb),
                "getDecoder().wrap(InputStream) must stream the original five bytes back");

        // getMimeEncoder(int, byte[]) argument handling — three separately
        // specified behaviours behind one signature.
        check(Base64.getMimeEncoder(19, new byte[] { '\n' }).encodeToString(big).length() == 84,
                "getMimeEncoder(19) must ROUND the line length DOWN to a multiple of four"
                        + " (19 >> 2 << 2 == 16), giving 84 characters");
        check(Base64.getMimeEncoder(16, new byte[] { '\n' }).encodeToString(big).length() == 84,
                "and getMimeEncoder(16) must agree exactly — the row that proves the rounding"
                        + " happened rather than 19 being honoured");
        check(Base64.getMimeEncoder(76, new byte[0]).encodeToString(big).length() == 80,
                "an EMPTY separator is legal and produces no separators at all");
        String[] badSep = { "A", "=" };
        for (int k = 0; k < badSep.length; k++) {
            Throwable st = null;
            try {
                sink = Base64.getMimeEncoder(76, badSep[k].getBytes(
                        java.nio.charset.StandardCharsets.ISO_8859_1)).hashCode();
            } catch (Throwable x) {
                st = x;
            }
            check("java.lang.IllegalArgumentException".equals(nameOf(st)),
                    "a line separator containing the base64-alphabet character '" + badSep[k]
                            + "' must be rejected; got " + nameOf(st));
        }
        check(Base64.getMimeEncoder(76, new byte[] { '-' }) != null,
                "'-' is NOT in the basic alphabet, so it is a legal separator — the negative"
                        + " control for the two rows above");

        // Encode content and the padding quantum across every residue class.
        check("+/+/AAE=".equals(new String(Base64.getEncoder().encode(bb),
                        java.nio.charset.StandardCharsets.ISO_8859_1)),
                "encode([B) must produce the same BYTES encodeToString produces characters"
                        + " for — the row above it only checked the length");
        check("AA==".equals(Base64.getEncoder().encodeToString(new byte[] { 0 })),
                "a 1-byte input must pad with TWO '='");
        check("AAA=".equals(Base64.getEncoder().encodeToString(new byte[] { 0, 0 })),
                "a 2-byte input must pad with ONE '='");
        check("AAAAAA==".equals(Base64.getEncoder().encodeToString(new byte[] { 0, 0, 0, 0 })),
                "a 4-byte input is one full quantum plus one byte: eight characters, two '='");
        check("-_-_AAE".equals(Base64.getUrlEncoder().withoutPadding().encodeToString(bb)),
                "withoutPadding() on the URL encoder must keep the URL ALPHABET — a copy that"
                        + " rebuilds from a variant tag loses one of the two settings");
        check(Base64.getEncoder().withoutPadding() != Base64.getEncoder().withoutPadding(),
                "withoutPadding() must return a FRESH object each call — the factories are"
                        + " singletons, this derived encoder deliberately is not");
        check(Base64.getDecoder().decode("QQ==") != Base64.getDecoder().decode("QQ=="),
                "decode(String) must return a fresh array, never a shared buffer");

        sectionEnd("b64", 60);
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: the constructor, toString, the two bit accessors,
        // version/variant/hashCode/equals, fromString and randomUUID. Never
        // reached: compareTo, nameUUIDFromBytes, and the three version-1 field
        // accessors — which is to say, every method whose answer is NOT a
        // rearrangement of the two longs.
        //
        // OBJECT-STATE NOTE: the rows above construct exactly one KIND of UUID
        // (a hand-built one, plus randomUUID). A version-1 UUID is a distinct
        // state with three accessors that WORK, and every other version is a
        // state where the same three accessors must THROW. Neither was built.
        // ===================================================================

        // GAP 1: compareTo is SIGNED on each long, so it does NOT agree with
        // the lexicographic order of toString. This is the sharpest row in the
        // family: a body that compares the hex text, or that compares the bits
        // unsigned, gets the opposite answer.
        UUID cLo = new UUID(OPAQUE_J[4], OPAQUE_J[3]);
        UUID cHi = new UUID(OPAQUE_J[2], OPAQUE_J[3]);
        check(cLo.compareTo(cHi) == 1,
                "new UUID(1, 0).compareTo(new UUID(-1, 0)) must be 1 — msb is compared as a"
                        + " SIGNED long, so 0xffff... is the SMALLER one");
        check(cLo.toString().compareTo(cHi.toString()) < 0,
                "and the two toString()s order the OTHER way — the row that proves compareTo is"
                        + " not implemented over the text");
        check(new UUID(OPAQUE_J[3], OPAQUE_J[2]).compareTo(new UUID(OPAQUE_J[3], OPAQUE_J[4])) == -1,
                "with equal msb the SIGNED lsb decides: lsb -1 sorts BEFORE lsb 1");
        check(new UUID(OPAQUE_J[4], OPAQUE_J[4]).compareTo(new UUID(OPAQUE_J[4], OPAQUE_J[4])) == 0,
                "compareTo must be 0 for equal values");
        check(new UUID(OPAQUE_J[3], OPAQUE_J[3]).compareTo(new UUID(OPAQUE_J[3], OPAQUE_J[3])) == 0,
                "the nil UUID must compare equal to itself");

        // GAP 2: nameUUIDFromBytes — an MD5 digest folded into a version-3
        // UUID. Fully deterministic, and the only method in this class whose
        // answer is not derivable from its argument by rearrangement.
        UUID nil = UUID.nameUUIDFromBytes(new byte[0]);
        check("d41d8cd9-8f00-3204-a980-0998ecf8427e".equals(nil.toString()),
                "UUID.nameUUIDFromBytes(new byte[0]) must be the MD5 of the empty input with"
                        + " the version and variant bits overwritten");
        check(nil.version() == 3, "nameUUIDFromBytes must set version 3");
        check(nil.variant() == 2, "nameUUIDFromBytes must set variant 2");
        check("97063c91-34aa-31b3-b933-47b92b5ae65d".equals(
                        UUID.nameUUIDFromBytes("cratonvm".getBytes(
                                java.nio.charset.StandardCharsets.US_ASCII)).toString()),
                "nameUUIDFromBytes(\"cratonvm\") must be its own fixed digest");
        Throwable ut = null;
        try {
            sink = UUID.nameUUIDFromBytes(null).version();
        } catch (Throwable x) {
            ut = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(ut)),
                "nameUUIDFromBytes(null) must throw NullPointerException, got " + nameOf(ut));

        // GAP 3: the version-1 accessors. THREE methods that answer on one
        // object state and throw on every other, and neither state was built.
        check(v1.timestamp() == 130742845922168750L,
                "the RFC 4122 example's timestamp() must be its 60 reassembled time bits");
        check(v1.clockSequence() == 10085, "its clockSequence() must be 10085");
        check(v1.node() == 690568981494L, "its node() must be the low 48 bits of the lsb");
        String[] v1Only = { "timestamp", "clockSequence", "node" };
        for (int k = 0; k < v1Only.length; k++) {
            Throwable vt = null;
            try {
                if (k == 0) {
                    sink = (int) u.timestamp();
                } else if (k == 1) {
                    sink = u.clockSequence();
                } else {
                    sink = (int) u.node();
                }
            } catch (Throwable x) {
                vt = x;
            }
            check("java.lang.UnsupportedOperationException".equals(nameOf(vt)),
                    "UUID." + v1Only[k] + "() on a version-6 UUID must throw"
                            + " UnsupportedOperationException — NOT IllegalArgument and NOT a"
                            + " wrong answer; got " + nameOf(vt));
        }

        // GAP 4: the remaining fromString rejections and equals against a
        // foreign type.
        check(!u.equals("00112233-4455-6677-8899-aabbccddeeff"),
                "UUID.equals(String) must be false even when the STRING is this UUID's own"
                        + " toString — equals is type-exact");
        ut = null;
        try {
            sink = UUID.fromString(null).version();
        } catch (Throwable x) {
            ut = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(ut)),
                "UUID.fromString(null) must throw NullPointerException, got " + nameOf(ut));
        ut = null;
        try {
            sink = UUID.fromString("-1-2-3-4-5").version();
        } catch (Throwable x) {
            ut = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ut)),
                "UUID.fromString(\"-1-2-3-4-5\") must throw IllegalArgumentException — a"
                        + " LEADING dash makes it six groups, the first of them empty; got "
                        + nameOf(ut));
        ut = null;
        try {
            sink = UUID.fromString("1-2-3-4").version();
        } catch (Throwable x) {
            ut = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(ut)),
                "UUID.fromString(\"1-2-3-4\") must throw — FOUR groups, where five is the rule"
                        + " that makes \"1-2-3-4-5\" legal above; got " + nameOf(ut));
        ut = null;
        try {
            sink = UUID.fromString("1-2-3-4-55555555555555555").version();
        } catch (Throwable x) {
            ut = x;
        }
        check("java.lang.NumberFormatException".equals(nameOf(ut)),
                "a node group of SEVENTEEN hex digits must throw NumberFormatException — the"
                        + " group count is right, so it is the unsigned-long parse that fails,"
                        + " exactly like the non-hex row above; got " + nameOf(ut));

        sectionEnd("uuid", 51);
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: the seven no-argument / single-bound draws and
        // nextBytes. Never reached: the ORIGIN-AND-BOUND overloads, the three
        // primitive STREAMS, nextExponential, and the no-arg constructor.
        //
        // OBJECT-STATE NOTE — the important one. Every Random above is FRESH or
        // mid-stream. java.util.Random has a third state: a PENDING GAUSSIAN
        // PARTNER, and the row above that claims "setSeed must RESET the
        // stream, INCLUDING the Gaussian cache" does not test the cache at all
        // — it draws nextInt(), which the cache never touches. [nextGaus] is a
        // prior finding in exactly this family, so the untested half of its own
        // claim is closed here.
        //
        // VALUE-DOMAIN NOTE: nextBytes was driven at length 7 only, chosen
        // because it is not a multiple of four. The multiple-of-four case is
        // the one where the tail loop must NOT run, and it was never asked.
        // ===================================================================

        // GAP 1: the pending-partner state, three ways.
        Random ga = new Random(OPAQUE_J[8]);
        long firstGaussian = Double.doubleToRawLongBits(ga.nextGaussian());
        ga.setSeed(OPAQUE_J[8]);
        check(Double.doubleToRawLongBits(ga.nextGaussian()) == firstGaussian,
                "setSeed must DISCARD a pending Gaussian partner: after setSeed(42) the next"
                        + " nextGaussian() must be #1 again, not the cached second value —"
                        + " the half of the setSeed row above that it never tested");
        Random gb = new Random(OPAQUE_J[8]);
        sink = (int) Double.doubleToRawLongBits(gb.nextGaussian());
        gb.setSeed(OPAQUE_J[8]);
        check(gb.nextInt() == -1170105035,
                "and setSeed from the pending state must also reset the INTEGER stream");
        Random gc = new Random(OPAQUE_J[8]);
        sink = (int) Double.doubleToRawLongBits(gc.nextGaussian());
        check(gc.nextInt() == 1325939940,
                "nextGaussian() must consume exactly the draws the polar method specifies, so"
                        + " the FOLLOWING nextInt() is the stream's fifth 32-bit draw"
                        + " (1325939940) and not its second (234785527)");

        // GAP 2: nextBytes at the lengths the tail loop treats differently.
        byte[] nb0 = new byte[0];
        new Random(OPAQUE_J[8]).nextBytes(nb0);
        check(nb0.length == 0, "nextBytes(new byte[0]) must be a no-op, not a panic");
        byte[] nb4 = new byte[4];
        new Random(OPAQUE_J[8]).nextBytes(nb4);
        check(Arrays.equals(nb4, new byte[] { 53, -99, 65, -70 }),
                "nextBytes(new byte[4]) — a length that is EXACTLY one draw, so the tail loop"
                        + " must not run");
        byte[] nb8 = new byte[8];
        new Random(OPAQUE_J[8]).nextBytes(nb8);
        check(Arrays.equals(nb8, new byte[] { 53, -99, 65, -70, -9, -118, -2, 13 }),
                "nextBytes(new byte[8]) — two whole draws");
        Throwable rt = null;
        try {
            new Random(OPAQUE_J[8]).nextBytes(null);
        } catch (Throwable x) {
            rt = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(rt)),
                "nextBytes(null) must throw NullPointerException, got " + nameOf(rt));

        // GAP 3: the origin-and-bound overloads and nextExponential. These are
        // RandomGenerator defaults composed from the specified primitives
        // above, so their outputs are fixed once the primitives are.
        Random ro = new Random(OPAQUE_J[8]);
        int[] wantRange = { 10, 13, 18, 14, 10 };
        for (int k = 0; k < wantRange.length; k++) {
            check(ro.nextInt(10, 20) == wantRange[k],
                    "new Random(42).nextInt(10, 20) #" + (k + 1) + " is determined by the"
                            + " specified nextInt() stream");
        }
        Random rlb = new Random(OPAQUE_J[8]);
        check(rlb.nextLong(100L) == 91L && rlb.nextLong(100L) == 40L && rlb.nextLong(100L) == 97L,
                "new Random(42).nextLong(100) must be 91, 40, 97");
        Random rlr = new Random(OPAQUE_J[8]);
        check(rlr.nextLong(10L, 20L) == 11L && rlr.nextLong(10L, 20L) == 10L,
                "new Random(42).nextLong(10, 20) must be 11 then 10");
        check(Double.doubleToRawLongBits(new Random(OPAQUE_J[8]).nextDouble(OPAQUE_SM[0]))
                        == 0x3ff74833a06ff457L,
                "new Random(42).nextDouble(2.0) #1 is the unbounded draw scaled, to the bit");
        check(Double.doubleToRawLongBits(
                        new Random(OPAQUE_J[8]).nextDouble(OPAQUE_SM[2], OPAQUE_SM[0]))
                        == 0x3ffba419d037fa2cL,
                "new Random(42).nextDouble(1.0, 2.0) #1 is specified, to the bit");
        check(Float.floatToRawIntBits(new Random(OPAQUE_J[8]).nextFloat(2.0f)) == 0x3fba419d,
                "new Random(42).nextFloat(2.0f) #1 is specified, to the bit");
        check(Double.doubleToRawLongBits(new Random(OPAQUE_J[8]).nextExponential())
                        == 0x3fc609c423733706L,
                "new Random(42).nextExponential() #1 is specified, to the bit");

        // GAP 4: the primitive streams. Their first N values must be exactly
        // the first N values of the corresponding scalar draw — a stream that
        // reseeds, buffers or reorders fails here and nowhere above.
        check(Arrays.equals(new Random(OPAQUE_J[8]).ints(5).toArray(),
                        new int[] { -1170105035, 234785527, -1360544799, 205897768, 1325939940 }),
                "Random.ints(5) must be the SAME five values nextInt() produces, in order");
        check(Arrays.equals(new Random(OPAQUE_J[8]).ints(5, 0, 100).toArray(),
                        new int[] { 30, 63, 48, 84, 70 }),
                "Random.ints(5, 0, 100) must agree with the nextInt(100) rows above");
        check(Arrays.equals(new Random(OPAQUE_J[8]).longs(3).toArray(),
                        new long[] { -5025562857975149833L, -5843495416241995736L,
                            5694868678511409995L }),
                "Random.longs(3) must be the nextLong() stream");
        double[] ds = new Random(OPAQUE_J[8]).doubles(2).toArray();
        check(Double.doubleToRawLongBits(ds[0]) == 0x3fe74833a06ff457L
                        && Double.doubleToRawLongBits(ds[1]) == 0x3fe5dcf778622e01L,
                "Random.doubles(2) must be the nextDouble() stream, to the bit");

        // GAP 5: the no-argument constructor and the remaining rejections.
        check(new Random().nextLong() != new Random().nextLong(),
                "two default-constructed Randoms must not share a seed — the uniquifier has to"
                        + " actually vary");
        check(new Random(OPAQUE_J[8]).nextInt(Integer.MAX_VALUE) == 1562431130,
                "nextInt(MAX_VALUE) — a bound that is not a power of two and fills the range,"
                        + " so the rejection loop is exercised at its widest");
        rt = null;
        try {
            sink = new Random(OPAQUE_J[8]).nextInt(OPAQUE_I[12], OPAQUE_I[12]);
        } catch (Throwable x) {
            rt = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(rt)),
                "nextInt(10, 10) must throw IllegalArgumentException — an EMPTY range, got "
                        + nameOf(rt));
        rt = null;
        try {
            sink = (int) new Random(OPAQUE_J[8]).nextLong(OPAQUE_J[3]);
        } catch (Throwable x) {
            rt = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(rt)),
                "nextLong(0) must throw IllegalArgumentException, got " + nameOf(rt));

        sectionEnd("random", 60);
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

        // ===================================================================
        // E26 — the reach audit.
        //
        // REACH BEFORE: format (both overloads), formatted, chars, lines,
        // indent, repeat, replace/replaceAll/replaceFirst, matches, valueOf,
        // transform. Never reached: split and join (a fourth grammar), the
        // strip family, isBlank, compareTo, equalsIgnoreCase, the LOCALE case
        // mappings, getBytes, and half the format conversions.
        //
        // OBJECT-STATE NOTE: a java.lang.String has a hidden state — its CODER.
        // A string whose characters all fit Latin-1 is stored one byte per
        // char; anything else is UTF-16. Every string above is ASCII or carries
        // surrogates; the LATIN-1-BUT-NOT-ASCII case (U+00E9) is a third path
        // and was never built.
        //
        // CROSS-FAMILY ROW: charcls pins Character.toUpperCase(U+00DF) == U+00DF
        // because "SS" does not fit a char. String.toUpperCase on the SAME code
        // point must return a string of length 2. One Unicode rule, two answers,
        // and a body that shares a case-mapping table between them cannot give
        // both.
        // ===================================================================

        // GAP 1: split / join — trailing empty strings are DISCARDED unless the
        // limit is negative, which is the rule no split implementation in any
        // other language shares.
        check("a,b,,".split(",").length == 2,
                "\"a,b,,\".split(\",\") must be TWO elements — trailing empties are dropped");
        check("a,b,,".split(",", -1).length == 4,
                "the same input with limit -1 must be FOUR — the limit is what preserves them");
        check("a,b,c".split(",", 2).length == 2 && "b,c".equals("a,b,c".split(",", 2)[1]),
                "a positive limit must stop splitting and leave the remainder intact");
        check("".split(",").length == 1 && "".equals("".split(",")[0]),
                "\"\".split(\",\") must be a ONE-element array holding the empty string — not"
                        + " an empty array, which is what \"\".lines() gives above");
        check(",a".split(",").length == 2 && "".equals(",a".split(",")[0]),
                "a LEADING separator must produce a leading empty element — only TRAILING"
                        + " empties are dropped");
        check("abc".split("").length == 3,
                "splitting on the empty pattern must give one element per character");
        check("a-b".equals(String.join("-", "a", "b")), "String.join must interleave");
        check("".equals(String.join("-")), "String.join with no elements must be empty");

        // GAP 2: strip vs trim. THREE different notions of blank in one class.
        check("\u2000x".equals("\u2000x".trim()),
                "trim() must NOT strip U+2000 — trim's rule is codepoint <= ' ', and U+2000 is"
                        + " above it");
        check("x".equals("\u2000x".strip()),
                "strip() MUST strip U+2000 — the same input, the other method");
        check("\u00a0x".equals("\u00a0x".strip()),
                "strip() must NOT strip U+00A0 — NBSP is Zs but is not Character.isWhitespace,"
                        + " and charcls pins that exact pair of answers above");
        check("x".equals("\u0001x\u0001".trim()),
                "trim() must strip U+0001 — a control character is <= ' '");
        check("x  ".equals("  x  ".stripLeading()), "stripLeading must touch only the front");
        check("  x".equals("  x  ".stripTrailing()), "stripTrailing must touch only the end");
        check("\u2000".isBlank(), "\"\\u2000\".isBlank() must be true");
        check(!"\u00a0".isBlank(),
                "\"\\u00a0\".isBlank() must be FALSE — isBlank follows strip, not trim");
        check("".isBlank(), "\"\".isBlank() must be true");

        // GAP 3: case mapping that CHANGES LENGTH, and compareTo's raw value.
        check("SS".equals("\u00df".toUpperCase(Locale.ROOT)),
                "\"\\u00df\".toUpperCase() must be \"SS\" — TWO characters from one, while"
                        + " Character.toUpperCase(U+00DF) is U+00DF in charcls above");
        check("\u00df".toUpperCase(Locale.ROOT).length() == 2,
                "and the length must actually grow — a char-by-char mapping cannot do this");
        check("FF".equals("\ufb00".toUpperCase(Locale.ROOT)),
                "\"\\ufb00 LATIN SMALL LIGATURE FF\".toUpperCase() must be \"FF\"");
        check("i".equals("I".toLowerCase(Locale.ROOT)),
                "\"I\".toLowerCase(ROOT) must be \"i\" — the locale-neutral answer");
        check("a".compareTo("B") == 31,
                "\"a\".compareTo(\"B\") must be 31 — the raw CHAR DIFFERENCE, not a normalised"
                        + " -1/0/1, which is the shape a Rust Ord-based body returns");
        check("a".compareToIgnoreCase("B") == -1,
                "\"a\".compareToIgnoreCase(\"B\") must be -1 — same operands, and here the"
                        + " difference really is -1");
        check("ab".compareTo("abc") == -1,
                "a prefix must compare by LENGTH DIFFERENCE when the common part is equal");
        check(!"\u00df".equalsIgnoreCase("SS"),
                "\"\\u00df\".equalsIgnoreCase(\"SS\") must be FALSE — equalsIgnoreCase is"
                        + " per-character, so it does NOT agree with toUpperCase four rows up");
        check("I".equalsIgnoreCase("i"), "\"I\".equalsIgnoreCase(\"i\") must be true");

        // GAP 4: bytes and the coder. The lone-surrogate row is the one a Rust
        // `String` cannot even represent: Java replaces it with '?' (0x3F) on
        // the way out, NOT with U+FFFD.
        check(Arrays.equals("\ud800".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                        new byte[] { 63 }),
                "an unpaired high surrogate must encode to UTF-8 as the single byte '?' (0x3F)"
                        + " — the encoder's unmappable-character replacement, not U+FFFD");
        check(Arrays.equals("a\ud800b".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                        new byte[] { 97, 63, 98 }),
                "and the surrounding characters must survive intact");
        check(Arrays.equals("\u00e9".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                        new byte[] { -61, -87 }),
                "U+00E9 must be TWO bytes in UTF-8");
        check(Arrays.equals("\u00e9".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1),
                        new byte[] { -23 }),
                "and ONE byte in ISO-8859-1 — the row that proves the charset is read");
        check("\ufffda".equals(new String(new byte[] { (byte) 0xff, 0x61 },
                        java.nio.charset.StandardCharsets.UTF_8)),
                "DECODING an invalid UTF-8 byte must give U+FFFD — the opposite replacement"
                        + " from the encoding direction three rows above");
        check("a\u00e9ba\u00e9b".equals("a\u00e9b".repeat(2)),
                "repeat() on a LATIN-1-but-not-ASCII string — the compact-string path no other"
                        + " row in this file takes");
        check("a\u4e00ba\u4e00b".equals("a\u4e00b".repeat(2)),
                "repeat() on a UTF-16 string must agree");
        check("a\u00e9b".indexOf(0xe9) == 1, "indexOf(int) must find a Latin-1 code point");
        check("a\ud83d\ude00b".indexOf(OPAQUE_CP[9]) == 1,
                "indexOf(int) must find an ASTRAL code point at its CODE UNIT index");
        check("\ud800".codePoints().toArray()[0] == 55296,
                "codePoints() over a lone surrogate must yield 55296, not U+FFFD");

        // GAP 5: the format conversions and the exception classes they raise.
        // Three MORE distinct throwables from one method.
        check(String.format("%c", Integer.valueOf(OPAQUE_CP[9])).length() == 2,
                "%c over an ASTRAL code point must emit a surrogate PAIR — two chars from one"
                        + " conversion");
        check("0x1.0p0".equals(String.format(Locale.ROOT, "%a", Double.valueOf(OPAQUE_SM[2]))),
                "%a must be the hex-float form, agreeing with Double.toHexString in floatfmt");
        check("37777777777".equals(String.format(Locale.ROOT, "%o", Integer.valueOf(OPAQUE_I[2]))),
                "%o of -1 must be UNSIGNED octal");
        check("010".equals(String.format(Locale.ROOT, "%#o", Integer.valueOf(8))),
                "the '#' flag on %o must prefix a zero");
        check("0xff".equals(String.format(Locale.ROOT, "%#x", Integer.valueOf(OPAQUE_I[7]))),
                "the '#' flag on %x must prefix \"0x\"");
        check("FF".equals(String.format(Locale.ROOT, "%X", Integer.valueOf(OPAQUE_I[7]))),
                "%X must upper-case the DIGITS");
        check("61".equals(String.format("%h", "a")),
                "%h must be the hex of hashCode() — 97 is 0x61");
        check("null".equals(String.format("%h", (Object) null)),
                "%h of null must be the string \"null\", NOT the hash of anything");
        check("   ab".equals(String.format("%5.2s", "abcdef")),
                "a PRECISION on %s must TRUNCATE before the width pads");
        check("(1,234.50)".equals(
                        String.format(Locale.ROOT, "%,(.2f", Double.valueOf(-1234.5))),
                "',' and '(' must compose: grouped digits inside parentheses, no minus sign");
        check("-003.140".equals(String.format(Locale.ROOT, "%08.3f", Double.valueOf(-3.14))),
                "zero padding must go AFTER the minus sign, not before it");
        check("1.235e-04".equals(String.format(Locale.ROOT, "%.3e", Double.valueOf(0.000123456))),
                "%.3e must round the significand and keep a two-digit exponent");
        check("1e+01".equals(String.format(Locale.ROOT, "%.0e", Double.valueOf(9.9))),
                "%.0e of 9.9 must carry into the exponent and emit NO decimal point");
        check("x x".equals(String.format("%s %<s", "x")),
                "the '<' relative index must re-use the PREVIOUS argument");
        String[] badFmt2 = { "%.2d", "%0$s" };
        String[] wantEx2 = {
            "java.util.IllegalFormatPrecisionException",
            "java.util.IllegalFormatArgumentIndexException",
        };
        for (int k = 0; k < badFmt2.length; k++) {
            Throwable ft = null;
            try {
                sink = String.format(Locale.ROOT, badFmt2[k], Integer.valueOf(1)).length();
            } catch (Throwable x) {
                ft = x;
            }
            check(wantEx2[k].equals(nameOf(ft)),
                    "String.format(\"" + badFmt2[k] + "\") must throw " + wantEx2[k]
                            + " — a DISTINCT subclass, not the generic one; got " + nameOf(ft));
        }
        t = null;
        try {
            sink = String.format("%c", Integer.valueOf(OPAQUE_CP[13])).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.IllegalFormatCodePointException".equals(nameOf(t)),
                "%c over 0x110000 must throw IllegalFormatCodePointException, got " + nameOf(t));
        t = null;
        try {
            sink = String.format(Locale.ROOT, (String) null).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "String.format(locale, null) must throw NullPointerException — a null FORMAT"
                        + " is not a format error; got " + nameOf(t));

        sectionEnd("strfmt", 114);
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
        check("AB\u0000\u0000".equals(be.toString()),
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

        // E8-1 N1 (APPLIED by E39) — ... and the other half of the same
        // expression: the JDK's four-term `||` SHORT-CIRCUITS, so `other` is
        // only dereferenced by the fourth term. A null `other` behind a failing
        // earlier term is a plain `false`, NOT a throw — measured on OpenJDK
        // 25.0.3+9. A fix that checks null first passes the row above and fails
        // these two.
        step("bounds", "String.regionMatches(bad toffset, null)");
        t = null;
        boolean shortCircuited = false;
        try {
            shortCircuited = !"ab".regionMatches(OPAQUE_I[13], null, OPAQUE_I[3], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && shortCircuited,
                "\"ab\".regionMatches(9, null, 0, 1) must answer FALSE without touching `other` —"
                        + " term three (toffset > length() - len) decides it; got " + nameOf(t));
        step("bounds", "String.regionMatches(negative ooffset, null)");
        t = null;
        shortCircuited = false;
        try {
            shortCircuited = !"ab".regionMatches(OPAQUE_I[3], null, OPAQUE_I[2], OPAQUE_I[4]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && shortCircuited,
                "\"ab\".regionMatches(0, null, -1, 1) must answer FALSE — term one (ooffset < 0)"
                        + " decides it before `other` is read; got " + nameOf(t));

        // ===================================================================
        // E26 — the reach audit. Every step line below still precedes its call.
        //
        // REACH BEFORE: AtomicReferenceArray's six plain accessors and the
        // int-capacity constructor; CharBuffer's get/charAt/toString/remaining/
        // hasArray. Never reached: the ARRAY constructor, toString, the six
        // functional updaters, compareAndExchange and the whole
        // plain/opaque/acquire/release mode surface.
        //
        // OBJECT-STATE NOTE — the important one for this family. Every
        // CharBuffer above is `CharBuffer.wrap(String)` or a ByteBuffer view.
        // A String-wrapped buffer is READ-ONLY and has NO accessible array; an
        // allocate()d one is writable and does. The fixture asserts hasArray()
        // == false for the view and never builds the state where it is true,
        // and never once calls put() — so nothing here has ever checked that a
        // write into a read-only buffer is refused.
        //
        // TRAP: the failure classes are deliberately NOT uniform. An index is
        // IndexOutOfBounds, a POSITION is IllegalArgumentException, a write to
        // a read-only buffer is ReadOnlyBufferException, an over-read is
        // BufferUnderflowException and a missing mark is InvalidMarkException.
        // Five classes, one class hierarchy, and none of them interchangeable.
        // ===================================================================

        // GAP 1: AtomicReferenceArray's array constructor and toString.
        AtomicReferenceArray<String> fromArray =
                new AtomicReferenceArray<>(new String[] { "a", "b", "c" });
        check(fromArray.length() == 3, "the E[] constructor must take its length from the array");
        check("b".equals(fromArray.get(OPAQUE_I[4])), "and its contents");
        check("[a, b, c]".equals(fromArray.toString()),
                "AtomicReferenceArray.toString must be the element list, not an identity hash");
        check("[]".equals(new AtomicReferenceArray<String>(new String[0]).toString()),
                "an empty one must print \"[]\"");
        String[] backing = { "a", "b" };
        AtomicReferenceArray<String> copied = new AtomicReferenceArray<>(backing);
        backing[0] = "MUTATED";
        check("a".equals(copied.get(OPAQUE_I[3])),
                "the E[] constructor must COPY — mutating the source array afterwards must not"
                        + " be visible, which a body that keeps the caller's pointer fails");
        t = null;
        try {
            sink = new AtomicReferenceArray<String>((String[]) null).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "new AtomicReferenceArray((E[]) null) must throw NullPointerException — where"
                        + " the int constructor's bad input is NegativeArraySizeException; got "
                        + nameOf(t));

        // GAP 2: the functional updaters and compareAndExchange.
        AtomicReferenceArray<String> fn = new AtomicReferenceArray<>(OPAQUE_I[5]);
        fn.set(OPAQUE_I[3], "x");
        check("x".equals(fn.getAndUpdate(OPAQUE_I[3], s -> s + "!")),
                "getAndUpdate must return the OLD value");
        check("x!".equals(fn.get(OPAQUE_I[3])), "and must have stored the new one");
        check("x!?".equals(fn.updateAndGet(OPAQUE_I[3], s -> s + "?")),
                "updateAndGet must return the NEW value — the opposite half of the same pair");
        check("x!?Z".equals(fn.accumulateAndGet(OPAQUE_I[3], "Z", (p, q) -> p + q)),
                "accumulateAndGet must apply the binary operator and return the new value");
        check("x!?Z".equals(fn.getAndAccumulate(OPAQUE_I[3], "W", (p, q) -> p + q)),
                "getAndAccumulate must return the OLD value");
        check("x!?ZW".equals(fn.get(OPAQUE_I[3])), "and must have stored the accumulation");
        String witness = fn.get(OPAQUE_I[3]);
        check("x!?ZW".equals(fn.compareAndExchange(OPAQUE_I[3], "wrong", "n")),
                "a FAILED compareAndExchange must return the WITNESSED value, not the expected"
                        + " one and not a boolean");
        // The comparison is REFERENCE identity, not equals. An equal-but-
        // distinct String must NOT succeed — measured on HotSpot 25, and a body
        // that compares by value writes here where the JDK does not.
        check(witness == fn.compareAndExchange(
                        OPAQUE_I[3], new String(witness.toCharArray()), "z"),
                "compareAndExchange with an EQUAL BUT DISTINCT expected reference must FAIL and"
                        + " return the witness");
        check(witness == fn.get(OPAQUE_I[3]),
                "and must not have written — the comparison is ==, never equals()");
        check(witness == fn.compareAndExchange(OPAQUE_I[3], witness, "n"),
                "a SUCCEEDING compareAndExchange must return the OLD REFERENCE");
        check("n".equals(fn.get(OPAQUE_I[3])), "and the write must have landed");
        check("n".equals(fn.getPlain(OPAQUE_I[3])) && "n".equals(fn.getOpaque(OPAQUE_I[3]))
                        && "n".equals(fn.getAcquire(OPAQUE_I[3])),
                "getPlain / getOpaque / getAcquire must all read the same value a plain get does");
        fn.setPlain(OPAQUE_I[4], "p");
        check("p".equals(fn.getPlain(OPAQUE_I[4])), "setPlain then getPlain must round-trip");
        fn.setRelease(OPAQUE_I[4], "r");
        check("r".equals(fn.getAcquire(OPAQUE_I[4])), "setRelease then getAcquire must round-trip");
        fn.setOpaque(OPAQUE_I[4], "o");
        check("o".equals(fn.getOpaque(OPAQUE_I[4])), "setOpaque then getOpaque must round-trip");
        check(fn.weakCompareAndSetPlain(OPAQUE_I[4], "o", "w"),
                "weakCompareAndSetPlain must succeed on the current value in a single thread");
        step("bounds", "AtomicReferenceArray.getAndUpdate(9)");
        t = null;
        try {
            fn.getAndUpdate(OPAQUE_I[13], s -> s);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "getAndUpdate(9, ..) must throw ArrayIndexOutOfBoundsException BEFORE it calls"
                        + " the function; got " + nameOf(t));
        step("bounds", "AtomicReferenceArray.getPlain(-1)");
        t = null;
        try {
            fn.getPlain(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "getPlain(-1) must throw ArrayIndexOutOfBoundsException — the RELAXED accessors"
                        + " are relaxed about MEMORY ORDER, not about bounds; got " + nameOf(t));
        step("bounds", "AtomicReferenceArray.compareAndExchange(9)");
        t = null;
        try {
            fn.compareAndExchange(OPAQUE_I[13], null, null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.ArrayIndexOutOfBoundsException".equals(nameOf(t)),
                "compareAndExchange(9, ..) must throw, got " + nameOf(t));

        // GAP 3: the CharBuffer STATES, and the five distinct failure classes.
        CharBuffer ro = CharBuffer.wrap("abcd");
        check(ro.isReadOnly(),
                "CharBuffer.wrap(String) must be READ-ONLY — a state no row above ever asked"
                        + " about, and the reason the next two rows throw");
        check(!ro.hasArray(), "a String-wrapped buffer must report hasArray() == false");
        check(ro.capacity() == 4, "and capacity 4");
        step("bounds", "CharBuffer.wrap(String).array()");
        t = null;
        try {
            sink = ro.array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "array() on a buffer with no accessible array must throw"
                        + " UnsupportedOperationException, got " + nameOf(t));
        step("bounds", "CharBuffer.wrap(String).put(0,'x')");
        t = null;
        try {
            ro.put(OPAQUE_I[3], 'x');
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "put() into a read-only buffer must throw ReadOnlyBufferException — NOT"
                        + " UnsupportedOperation and NOT a silent no-op; got " + nameOf(t));
        CharBuffer wr = CharBuffer.allocate(OPAQUE_I[6] + 1);
        check(!wr.isReadOnly(), "CharBuffer.allocate(4) must be WRITABLE — the other state");
        check(wr.hasArray() && wr.array().length == 4 && wr.arrayOffset() == 0,
                "and array-backed, with a zero offset");
        wr.put(OPAQUE_I[3], 'z');
        check(wr.get(OPAQUE_I[3]) == 'z', "put(0,'z') into a writable buffer must be readable back");
        step("bounds", "CharBuffer.allocate(4).put(9,'z')");
        t = null;
        try {
            wr.put(OPAQUE_I[13], 'z');
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "put(9,'z') past the end must throw IndexOutOfBoundsException, got " + nameOf(t));
        step("bounds", "CharBuffer.allocate(-1)");
        t = null;
        try {
            sink = CharBuffer.allocate(OPAQUE_I[2]).capacity();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "CharBuffer.allocate(-1) must throw IllegalArgumentException — where"
                        + " new AtomicReferenceArray(-1) is NegativeArraySizeException above;"
                        + " got " + nameOf(t));

        // GAP 3b: the THIRD accessible-array state — ARRAY-BACKED but
        // READ-ONLY. The two cells above are (read-only AND array-less) and
        // (writable AND array-backed); nothing in this file has ever asked what
        // happens when a buffer HAS an array it is not allowed to hand out.
        //
        // hasArray() is (hb != null) && !isReadOnly, and array()/arrayOffset()
        // are the SAME three-way split in the SAME order — hb first:
        //
        //     if (hb == null)  throw new UnsupportedOperationException();
        //     if (isReadOnly)  throw new ReadOnlyBufferException();
        //
        // (JDK 25 java.base/java/nio/CharBuffer.java L1490/L1513/L1541, and the
        // byte-identical ByteBuffer bodies at the same three line numbers.)
        // This is the cell where CratonVM's ByteBuffer natives classified
        // storage only and handed a MUTABLE ALIAS to the backing array out, to
        // a caller that had followed the documented hasArray() -> array()
        // protocol and been told true. A wrong CAPABILITY, not a wrong value,
        // and this fixture passed regardless.
        //
        // Every row below compares the EXACT class name, never instanceof:
        // ReadOnlyBufferException EXTENDS UnsupportedOperationException (JDK 25
        // java.base/java/nio/ReadOnlyBufferException.java:40), so an
        // instanceof-shaped assertion absorbs a swap in one direction and only
        // nameOf() can discriminate.
        CharBuffer roArr = CharBuffer.allocate(OPAQUE_I[6] + 1).asReadOnlyBuffer();
        check(roArr.isReadOnly(),
                "allocate(4).asReadOnlyBuffer() must be READ-ONLY — the premise of the next"
                        + " three rows, and the row that fails if asReadOnlyBuffer() hands"
                        + " the receiver back instead of a read-only view");
        check(!roArr.hasArray(),
                "a read-only ARRAY-BACKED buffer must report hasArray() == false — an"
                        + " implementation that only classifies storage as heap-or-direct"
                        + " answers true here and steers the caller straight into array()");
        step("bounds", "CharBuffer.allocate(4).asReadOnlyBuffer().array()");
        t = null;
        try {
            sink = roArr.array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "array() on a read-only ARRAY-BACKED buffer must throw"
                        + " ReadOnlyBufferException — NOT UnsupportedOperation, which is the"
                        + " array-LESS answer, and above all not the array itself; got "
                        + nameOf(t));
        step("bounds", "CharBuffer.allocate(4).asReadOnlyBuffer().arrayOffset()");
        t = null;
        try {
            sink = roArr.arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "arrayOffset() repeats array()'s split exactly — a read-only receiver must"
                        + " throw ReadOnlyBufferException, not answer a plain offset that is"
                        + " indistinguishable from a legitimate one; got " + nameOf(t));

        // ...and the FOURTH cell, which is what proves the split is NOT
        // "read-only versus not": a typed VIEW over a writable ByteBuffer is
        // itself WRITABLE and still has no accessible array, so it answers
        // UnsupportedOperation.
        IntBuffer vw = ByteBuffer.allocate(OPAQUE_I[11]).asIntBuffer();
        check(!vw.isReadOnly(),
                "asIntBuffer() over a WRITABLE ByteBuffer is itself writable — the premise"
                        + " that makes the next three rows a statement about hb rather than"
                        + " about read-only");
        check(!vw.hasArray(),
                "and it still reports hasArray() == false: a view buffer's hb is null");
        step("bounds", "ByteBuffer.allocate(16).asIntBuffer().array()");
        t = null;
        try {
            sink = vw.array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "array() on a WRITABLE array-less view must throw UnsupportedOperation —"
                        + " not ReadOnlyBuffer, and not a null array whose .length surfaces"
                        + " as a NullPointerException at some unrelated site; got "
                        + nameOf(t));
        check(t.getMessage() == null,
                "and HotSpot's is new UnsupportedOperationException() — the NO-ARGUMENT"
                        + " constructor, so getMessage() is null. A detail message is"
                        + " invisible to every catch and to every class-name row above, so"
                        + " this is the only assertion in the file that can see one; got "
                        + t.getMessage());
        step("bounds", "ByteBuffer.allocate(16).asIntBuffer().arrayOffset()");
        t = null;
        try {
            sink = vw.arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "arrayOffset() on the same writable view must throw UnsupportedOperation"
                        + " too — the identical split; got " + nameOf(t));

        // GAP 4: position / limit / slice — the state a buffer carries, and the
        // exception class that is NOT IndexOutOfBounds.
        CharBuffer win = CharBuffer.wrap("abcdef", OPAQUE_I[4], OPAQUE_I[6]);
        check(win.position() == 1 && win.limit() == 3 && win.remaining() == 2,
                "wrap(seq, 1, 3) must set position 1 and limit 3 — the second argument is a"
                        + " START and the third a LENGTH-derived END, not a length");
        check("bc".equals(win.toString()),
                "and its toString must be the REMAINING characters only");
        step("bounds", "CharBuffer.wrap(seq, 1, 9)");
        t = null;
        try {
            sink = CharBuffer.wrap("abcdef", OPAQUE_I[4], OPAQUE_I[13]).limit();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "wrap(seq, 1, 9) must throw IndexOutOfBoundsException, got " + nameOf(t));
        CharBuffer st = CharBuffer.wrap("abcd");
        step("bounds", "CharBuffer.position(9)");
        t = null;
        try {
            st.position(OPAQUE_I[13]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "position(9) must throw IllegalArgumentException — a POSITION is not an INDEX,"
                        + " and this is the row that separates the two contracts; got "
                        + nameOf(t));
        step("bounds", "CharBuffer.limit(9)");
        t = null;
        try {
            st.limit(OPAQUE_I[13]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "limit(9) must throw IllegalArgumentException, got " + nameOf(t));
        step("bounds", "CharBuffer.position(-1)");
        t = null;
        try {
            st.position(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "position(-1) must throw IllegalArgumentException, got " + nameOf(t));
        st.position(OPAQUE_I[4]);
        check(st.slice().get(OPAQUE_I[3]) == 'b',
                "slice() must start at the current POSITION");
        check(st.slice().capacity() == 3, "and its capacity must be the remaining count");
        check(st.duplicate().position() == 1, "duplicate() must carry the position over");
        check("bc".equals(st.subSequence(OPAQUE_I[3], OPAQUE_I[5]).toString()),
                "subSequence indices are RELATIVE TO THE POSITION, like charAt above");
        step("bounds", "CharBuffer.subSequence(0, 9)");
        t = null;
        try {
            sink = st.subSequence(OPAQUE_I[3], OPAQUE_I[13]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "subSequence(0, 9) must throw IndexOutOfBoundsException, got " + nameOf(t));
        CharBuffer walked = CharBuffer.wrap("abcd");
        walked.get();
        walked.get();
        check(walked.rewind().position() == 0, "rewind() must reset the position to zero");
        step("bounds", "CharBuffer relative get() past the limit");
        t = null;
        try {
            CharBuffer over = CharBuffer.wrap("abcd");
            for (int k = 0; k < 5; k++) {
                sink = over.get();
            }
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.BufferUnderflowException".equals(nameOf(t)),
                "a RELATIVE get() past the limit must throw BufferUnderflowException — where"
                        + " the ABSOLUTE get(9) above throws IndexOutOfBounds; got " + nameOf(t));
        step("bounds", "CharBuffer.reset() with no mark");
        t = null;
        try {
            CharBuffer.wrap("abcd").reset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.InvalidMarkException".equals(nameOf(t)),
                "reset() with no mark set must throw InvalidMarkException — the fifth distinct"
                        + " class in this family; got " + nameOf(t));

        // GAP 5: String's own most-called accessor, and a read-only ByteBuffer.
        step("bounds", "String.charAt(len)");
        t = null;
        try {
            sink = "ab".charAt(OPAQUE_I[5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".charAt(2) must throw StringIndexOutOfBoundsException, got " + nameOf(t));
        step("bounds", "String.charAt(-1)");
        t = null;
        try {
            sink = "ab".charAt(OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".charAt(-1) must throw StringIndexOutOfBoundsException, got " + nameOf(t));
        step("bounds", "String.substring(3)");
        t = null;
        try {
            sink = "ab".substring(OPAQUE_I[6]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".substring(3) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        check("".equals("ab".substring(OPAQUE_I[5])),
                "\"ab\".substring(2) must be the EMPTY string — one past the end is legal here,"
                        + " which charAt(2) two rows up rejects");
        step("bounds", "String.substring(2, 1)");
        t = null;
        try {
            sink = "ab".substring(OPAQUE_I[5], OPAQUE_I[4]).length();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "\"ab\".substring(2, 1) must throw StringIndexOutOfBoundsException, got "
                        + nameOf(t));
        step("bounds", "ByteBuffer.getChar(7) - one byte short of a char");
        t = null;
        try {
            sink = raw.getChar(7);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.IndexOutOfBoundsException".equals(nameOf(t)),
                "getChar(7) on an 8-byte buffer must throw — the LAST byte cannot start a"
                        + " two-byte read, so the bound is capacity-1, not capacity; got "
                        + nameOf(t));
        step("bounds", "ByteBuffer.asReadOnlyBuffer().putChar(0)");
        t = null;
        try {
            raw.asReadOnlyBuffer().putChar(OPAQUE_I[3], 'x');
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "putChar into a read-only ByteBuffer must throw ReadOnlyBufferException, got "
                        + nameOf(t));

        // GAP 3c: READ-ONLY IS CONTAGIOUS. GAP 3b asks what a read-only buffer
        // answers; nothing has ever asked what a buffer DERIVED from one
        // answers. MEASURED on jdk-25.0.3+9, all seven families, identical:
        // duplicate()/slice()/slice(int,int) INHERIT isReadOnly and
        // asReadOnlyBuffer() sets it unconditionally, so there is no
        // composition of buffer operations that returns to writable.
        //
        // This is where the wrong CAPABILITY GAP 3b closes re-opens ONE CALL
        // LATER: a duplicate that lost the flag answers hasArray() == true and
        // hands out the SAME backing array the read-only buffer wraps, so every
        // write through it corrupts a read-only buffer with no exception.
        //
        // APPENDED at the end of the family ON PURPOSE, exactly as the `hex`
        // family's F2-1 rows were: this file has a prediction table keyed by
        // check NUMBER, and inserting beside the GAP 3b rows would renumber
        // every row after them. These are checks 103-121 of `bounds`.
        //
        // Every row compares an EXACT class name. ReadOnlyBufferException
        // EXTENDS UnsupportedOperationException, so instanceof discriminates in
        // one direction only.
        CharBuffer roDup = roArr.duplicate();
        check(roDup.isReadOnly(),
                "duplicate() of a read-only buffer is read-only — rcb.duplicate()"
                        + " is a java.nio.HeapCharBufferR. An implementation that copies"
                        + " pos/lim/cap and writes nothing to isReadOnly answers false here");
        check(!roDup.hasArray(),
                "and therefore reports hasArray() == false — this is the row that"
                        + " stops the caller being steered into array() a second time");
        step("bounds", "CharBuffer.allocate(4).asReadOnlyBuffer().duplicate().array()");
        t = null;
        try {
            sink = roDup.array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "array() on the duplicate of a read-only buffer must throw"
                        + " ReadOnlyBufferException; a duplicate that lost the flag returns"
                        + " the array and this row sees no throwable at all; got " + nameOf(t));
        check(roArr.slice().isReadOnly(),
                "slice() is contagious too — rcb.slice() is a HeapCharBufferR");
        check(roArr.slice(0, 2).isReadOnly(),
                "and so is the absolute-indexed slice(int,int) overload, which is a"
                        + " SEPARATE registration and can be fixed independently");
        check(roDup.asReadOnlyBuffer().isReadOnly(),
                "asReadOnlyBuffer() is ONE-WAY: nothing in java.nio clears the flag,"
                        + " there is no asWritableBuffer, and a read-only buffer's own"
                        + " asReadOnlyBuffer() stays read-only");
        CharBuffer wArr = CharBuffer.allocate(OPAQUE_I[6] + 1);
        check(!wArr.duplicate().isReadOnly(),
                "and it is NOT contagious upward — hcb.duplicate().isReadOnly() is"
                        + " false. An implementation that stamped every derived view"
                        + " read-only passes every row above and fails this one");
        check(wArr.duplicate().hasArray(),
                "a writable duplicate keeps its accessible array");

        ByteBuffer roBb = ByteBuffer.allocate(OPAQUE_I[11]).asReadOnlyBuffer();
        check(roBb.duplicate().isReadOnly(),
                "the ByteBuffer half of the same contract — rbb.duplicate() is a"
                        + " java.nio.HeapByteBufferR. This is the family whose natives"
                        + " share the source's backing array, so the mutable alias here"
                        + " aliases the READ-ONLY buffer's own storage");
        check(!roBb.duplicate().hasArray(), "rbb.duplicate().hasArray() is false");
        step("bounds", "ByteBuffer.allocate(16).asReadOnlyBuffer().duplicate().array()");
        t = null;
        try {
            sink = roBb.duplicate().array().length;
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "rbb.duplicate().array() must throw ReadOnlyBufferException; got "
                        + nameOf(t));
        check(roBb.slice().isReadOnly(), "rbb.slice().isReadOnly()");
        step("bounds", "ByteBuffer.allocate(16).asReadOnlyBuffer().slice().arrayOffset()");
        t = null;
        try {
            sink = roBb.slice().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "arrayOffset() on a read-only slice repeats array()'s split — a plain"
                        + " offset here is indistinguishable from a legitimate one; got "
                        + nameOf(t));
        check(!ByteBuffer.allocate(OPAQUE_I[11]).duplicate().isReadOnly(),
                "hbb.duplicate().isReadOnly() is false");

        // The three rows that make the typed families' arrayOffset registration
        // load-bearing: arrayOffset was registered for ByteBuffer only, so on a
        // VM-minted IntBuffer it resolved to the Code-less java/nio/Buffer
        // declaration and threw AbstractMethodError.
        IntBuffer wIb = IntBuffer.allocate(OPAQUE_I[6] + 1);
        check(wIb.arrayOffset() == 0,
                "IntBuffer.allocate(4).arrayOffset() is 0 — an ANSWER, not an"
                        + " AbstractMethodError from an unregistered accessor");
        step("bounds", "IntBuffer.allocate(4).asReadOnlyBuffer().arrayOffset()");
        t = null;
        try {
            sink = wIb.asReadOnlyBuffer().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "the read-only cell of the typed families' arrayOffset; got " + nameOf(t));
        step("bounds", "IntBuffer.allocate(4).asReadOnlyBuffer().duplicate().arrayOffset()");
        t = null;
        try {
            sink = wIb.asReadOnlyBuffer().duplicate().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.nio.ReadOnlyBufferException".equals(nameOf(t)),
                "and the same cell reached through a DUPLICATE, which is the"
                        + " registration and the contagion in one row; got " + nameOf(t));
        step("bounds", "ByteBuffer.allocate(16).asIntBuffer().arrayOffset()");
        t = null;
        try {
            sink = ByteBuffer.allocate(OPAQUE_I[11]).asIntBuffer().arrayOffset();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.UnsupportedOperationException".equals(nameOf(t)),
                "a WRITABLE typed view has no array at all, so arrayOffset() answers"
                        + " UnsupportedOperation — the cell a read-only-first"
                        + " implementation gets wrong; got " + nameOf(t));
        check(t != null && t.getMessage() == null,
                "and that UnsupportedOperationException carries a NULL detail message."
                        + " This is one of only two assertions in this file that can see a"
                        + " wrong message; the CLASS being right is what let"
                        + " \"direct buffer has no backing array\" survive a prior repair");

        // 93 -> 102: F14 added GAP 3b, the nine rows for the read-only
        // ARRAY-BACKED cell and the writable ARRAY-LESS view cell. Re-derived
        // by running this family on jdk-25.0.3+9, not by adding 9 on paper.
        // 102 -> 121: F21 added GAP 3c, the nineteen read-only-contagion and
        // typed-arrayOffset rows. Same derivation: measured, not counted.
        sectionEnd("bounds", 121);
    }

    // -----------------------------------------------------------------------
    // 9b. strnull — String's REFERENCE-ARGUMENT contracts. Three nominations
    //     from two other lanes land here, applied by E39; each is measured on
    //     OpenJDK 25.0.3+9 and none of them is remembered.
    //
    //     (a) E8-1 N2 — the NULL-argument family. Nine String methods returned
    //     a plausible wrong VALUE for a null argument and no regression row
    //     noticed, because every row in `strfmt` and `bounds` passes valid
    //     arguments: `false` from the predicates, `-1` from the searches
    //     (indistinguishable from a real miss), a null String[] from split, ""
    //     from join and copyValueOf, the receiver from transform, a silent
    //     success from getChars.
    //
    //     The four non-throwing rows and the startsWith escape hatch are
    //     deliberate and load-bearing: "takes a reference" does NOT imply
    //     "throws", and a fix applied by SHAPE breaks equals/equalsIgnoreCase.
    //     Keep them. This is the same non-uniformity `bounds` pins for
    //     regionMatches, one method over.
    //
    //     (b) E18-1 N3 — the argument-TYPE test. `String.equals` is guarded by
    //     `instanceof String`; `contentEquals` is the sibling that compares
    //     across CharSequence types. The JIT's StringEquals intrinsic inlines a
    //     String-layout decode of the ARGUMENT guarded only against null, so a
    //     same-length StringBuilder is the operand that separates them.
    //
    //     (c) E18-1 N6 — `indexOf(int)`/`lastIndexOf(int)` do NOT narrow to a
    //     code unit; the gate is Character.isValidCodePoint, checked BEFORE any
    //     narrowing. Four disagreeing implementations of this one JVMS rule
    //     have been found in this tree (E18-1, E27-1), and until E26 nothing in
    //     the fixture called `lastIndexOf(int)` at all.
    //
    //     N6's own `mixed.indexOf(0x10437) == 3` row is NOT here: E26's
    //     `strfmt` row `indexOf(OPAQUE_CP[9]) == 1` on the astral-emoji
    //     receiver already asserts it, and the extra discrimination N6 carried
    //     — a receiver where the MASKED low half occurs EARLIER than the pair —
    //     is carried by the `lastIndexOf(0x10437, 2)` row below, whose receiver
    //     is the same string and whose backward scan passes over that very
    //     unit. Two rows asserting one contract is how a denominator drifts.
    // -----------------------------------------------------------------------
    static void strnull() {
        String s = "abc";
        String nul = (String) NULL_OBJ;

        // --- (a) E8-1 N2. The four that must NOT throw. ---
        check(!s.equals(NULL_OBJ), "\"abc\".equals(null) must be FALSE, not a throw");
        check(!s.equalsIgnoreCase(nul),
                "\"abc\".equalsIgnoreCase(null) must be FALSE, not a throw — the sibling"
                        + " compareToIgnoreCase(null) DOES throw");
        check("null".equals(String.valueOf(NULL_OBJ)),
                "String.valueOf((Object) null) must be the four-character string \"null\"");
        check("a,null,b".equals(String.join(",", "a", null, "b")),
                "a null ELEMENT of String.join must render as \"null\"");

        // The negative-offset escape hatch: startsWith has regionMatches's
        // short-circuiting shape, so this one is FALSE and not a throw.
        Throwable t = null;
        boolean neg = false;
        try {
            neg = !s.startsWith(nul, OPAQUE_I[2]);
        } catch (Throwable x) {
            t = x;
        }
        check(t == null && neg,
                "\"abc\".startsWith(null, -1) must be FALSE — toffset < 0 is checked before the"
                        + " prefix is dereferenced; got " + nameOf(t));

        // Everything else throws NullPointerException.
        checkNpe("contains", () -> s.contains(nul));
        checkNpe("startsWith", () -> s.startsWith(nul));
        checkNpe("startsWith(_,0)", () -> s.startsWith(nul, OPAQUE_I[3]));
        checkNpe("endsWith", () -> s.endsWith(nul));
        checkNpe("indexOf(String)", () -> s.indexOf(nul));
        checkNpe("lastIndexOf(String)", () -> s.lastIndexOf(nul));
        checkNpe("compareTo", () -> s.compareTo(nul));
        checkNpe("compareToIgnoreCase", () -> s.compareToIgnoreCase(nul));
        checkNpe("concat", () -> s.concat(nul));
        checkNpe("split", () -> s.split(nul));
        checkNpe("split(_,2)", () -> s.split(nul, OPAQUE_I[5]));
        checkNpe("matches", () -> s.matches(nul));
        checkNpe("replaceAll", () -> s.replaceAll(nul, "x"));
        checkNpe("transform", () -> s.transform(null));
        checkNpe("toUpperCase(Locale)", () -> s.toUpperCase((Locale) NULL_OBJ));
        checkNpe("toLowerCase(Locale)", () -> s.toLowerCase((Locale) NULL_OBJ));
        checkNpe("join(null delim)", () -> String.join(null, "a", "b"));
        checkNpe("join(null array)", () -> String.join(",", (CharSequence[]) NULL_OBJ));
        checkNpe("join(null iterable)", () -> String.join(",", NULL_ITER));
        checkNpe("copyValueOf", () -> String.copyValueOf((char[]) NULL_OBJ));
        checkNpe("valueOf(char[])", () -> String.valueOf((char[]) NULL_OBJ));
        checkNpe("new String(char[])", () -> new String((char[]) NULL_OBJ));
        checkNpe("getChars(null dst)", () -> {
            s.getChars(OPAQUE_I[3], OPAQUE_I[4], (char[]) NULL_OBJ, OPAQUE_I[3]);
            return null;
        });

        // getChars checks the SOURCE range before it looks at dst, so a bad
        // range plus a null dst is a StringIndexOutOfBoundsException — and the
        // destination-range failure is ALSO StringIndexOutOfBounds, not
        // ArrayIndexOutOfBounds.
        t = null;
        try {
            s.getChars(OPAQUE_I[3], OPAQUE_I[13], (char[]) NULL_OBJ, OPAQUE_I[3]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "getChars(0, 9, null, 0) must report the SOURCE range first, as"
                        + " StringIndexOutOfBoundsException; got " + nameOf(t));
        t = null;
        try {
            s.getChars(OPAQUE_I[3], OPAQUE_I[6], new char[OPAQUE_I[6]], OPAQUE_I[5]);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.StringIndexOutOfBoundsException".equals(nameOf(t)),
                "getChars(0, 3, new char[3], 2) must throw StringIndexOutOfBoundsException — NOT"
                        + " ArrayIndexOutOfBounds, String does its own checking; got " + nameOf(t));

        // --- (b) E18-1 N3. equals is TYPE-guarded; contentEquals is not. ---
        // A same-LENGTH non-String CharSequence: `new StringBuilder(3)` really
        // does have capacity 3 on OpenJDK 25.0.3+9, so an equals() that reads
        // the argument's value array by slot index matches it.
        StringBuilder sb3 = new StringBuilder(OPAQUE_I[6]);
        sb3.append("abc");
        check(!"abc".equals((Object) sb3),
                "String.equals(StringBuilder) must be FALSE — equals is guarded by"
                        + " `instanceof String`, and contentEquals is the method that says true");
        check("abc".contentEquals(sb3),
                "String.contentEquals(StringBuilder) must be TRUE — the sibling that DOES compare"
                        + " content across CharSequence types");

        // --- (c) E18-1 N6. indexOf(int) does not narrow to a code unit. ---
        // 0x10437 & 0xFFFF == 0x0437, and this receiver holds a real U+0437 at
        // index 1, EARLIER than the pair at 3 — so a masking implementation and
        // a first-hit-wins hybrid answer 1 where the JDK answers 3.
        // Both receivers are built from EXPLICIT code units rather than written
        // as non-ASCII source characters: run.sh compiles src/*.java with no
        // -encoding flag, so a literal operand would be decoded with whatever
        // the platform's native encoding happens to be. The array spelling also
        // says what the receiver IS — "x", U+0437, "y", the D801/DC37 pair, "z"
        // — which is the whole point of the rows below.
        String mixed = new String(
                new char[] { 'x', (char) 0x0437, 'y', (char) 0xd801, (char) 0xdc37, 'z' });
        String ffffq = new String(new char[] { (char) 0xffff, 'q' });
        check("abc".indexOf(0x10061) < 0,
                "\"abc\".indexOf(0x10061) must be -1 — a MASKING implementation finds 'a' at 0");
        check("abc".lastIndexOf(0x10061) < 0,
                "\"abc\".lastIndexOf(0x10061) must be -1 — same rule, backwards");
        check(mixed.lastIndexOf(0xDC37) == 4,
                "a LONE low surrogate is an ordinary code-unit scan — an implementation built on"
                        + " a code-point type answers -1 here");
        check(ffffq.indexOf(-1) < 0,
                "indexOf(-1) must be -1 even when the receiver holds U+FFFF: the gate is"
                        + " isValidCodePoint, not a narrowing cast to (char)");
        check(mixed.lastIndexOf(0x10437, OPAQUE_I[5]) < 0
                        && mixed.lastIndexOf(0x10437, OPAQUE_I[6]) == 3,
                "lastIndexOf(supplementary, from) starts at min(from, length - 2), not length - 1"
                        + " — and the fromIndex=2 half also rejects a scan that would settle for"
                        + " the masked low half sitting at index 1");

        sectionEnd("strnull", 37);
    }

    /** Opaque null, so javac cannot fold a null-argument call at compile time. */
    static final Object NULL_OBJ = null;

    /**
     * The same opaque null, pre-typed for {@code String.join}'s Iterable
     * overload. A {@code (Iterable<CharSequence>) NULL_OBJ} cast would select
     * the same overload but makes the whole file compile with an unchecked
     * warning, and run.sh compiles src/*.java in one javac invocation.
     */
    static final Iterable<CharSequence> NULL_ITER = null;

    /** Asserts that {@code body} throws exactly java.lang.NullPointerException. */
    static void checkNpe(String what, java.util.concurrent.Callable<Object> body) {
        Throwable t = null;
        try {
            body.call();
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "String." + what + " with a null argument must throw NullPointerException, got "
                        + nameOf(t));
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

        // ===================================================================
        // E26 — the reach audit. THE SHARPEST VALUE-DOMAIN HOLE IN THE FILE.
        //
        // Of the 34 rows above, thirty-two drive MIN_VALUE, MAX_VALUE, -0.0 or
        // -1. Not one drives an ordinary number. StrictMath's contract is that
        // it reproduces fdlibm BIT FOR BIT — so unlike Math, its answers on
        // ordinary inputs are fully specified and assertable — and none of them
        // has ever been asked. A body that dispatches to Rust's `f64::powf`
        // (which is the platform libm, not fdlibm) answers every one of the 34
        // rows above correctly and is wrong here.
        //
        // The complementary row is Math: its javadoc allows 1 ulp of error from
        // the exact result and requires semi-monotonicity, so Math.X is checked
        // AGAINST StrictMath.X with a 1-ulp budget rather than against a
        // literal. That is the assertion shape a 44-ulp fast path fails and a
        // legitimately-different-but-conforming implementation passes.
        //
        // Also never reached: absExact (the THROWING twin of the four wrapping
        // abs rows directly above), divideExact, the ceil/floor Exact forms,
        // clamp, multiplyFull, round/rint and the ulp/nextUp family.
        // ===================================================================

        // GAP 1: StrictMath on ordinary inputs, to the bit.
        check(Double.doubleToRawLongBits(StrictMath.pow(OPAQUE_SM[0], OPAQUE_SM[1]))
                        == 0x3ff6a09e667f3bcdL,
                "StrictMath.pow(2.0, 0.5) must be 0x3ff6a09e667f3bcd — fdlibm's answer, not"
                        + " merely one within an ulp of it");
        check(Double.doubleToRawLongBits(StrictMath.pow(OPAQUE_SM[4], OPAQUE_SM[15]))
                        == 0x4480f0cf064dd592L,
                "StrictMath.pow(10.0, 22.0) must be 0x4480f0cf064dd592 — the largest power of"
                        + " ten that is exact in a double, and the classic pow fast-path case");
        check(Double.doubleToRawLongBits(StrictMath.pow(OPAQUE_SM[3], OPAQUE_SM[16]))
                        == 0x43e517168a4523fdL,
                "StrictMath.pow(3.0, 40.0) must be 0x43e517168a4523fd");
        check(Double.doubleToRawLongBits(StrictMath.pow(OPAQUE_SM[1], OPAQUE_SM[1]))
                        == 0x3fe6a09e667f3bcdL,
                "StrictMath.pow(0.5, 0.5) must be 0x3fe6a09e667f3bcd");
        check(Double.doubleToRawLongBits(StrictMath.exp(OPAQUE_SM[2])) == 0x4005bf0a8b14576aL,
                "StrictMath.exp(1.0) must be 0x4005bf0a8b14576a — note this is NOT Math.E's"
                        + " bit pattern; fdlibm's exp(1) is one ulp above the constant");
        check(Double.doubleToRawLongBits(StrictMath.log(OPAQUE_SM[0])) == 0x3fe62e42fefa39efL,
                "StrictMath.log(2.0) must be 0x3fe62e42fefa39ef");
        check(Double.doubleToRawLongBits(StrictMath.log10(OPAQUE_SM[9])) == 0x4008000000000000L,
                "StrictMath.log10(1000.0) must be EXACTLY 3.0");
        check(Double.doubleToRawLongBits(StrictMath.sqrt(OPAQUE_SM[0])) == 0x3ff6a09e667f3bcdL,
                "StrictMath.sqrt(2.0) must be 0x3ff6a09e667f3bcd");
        check(Double.doubleToRawLongBits(StrictMath.cbrt(OPAQUE_SM[8])) == 0x4008000000000000L,
                "StrictMath.cbrt(27.0) must be EXACTLY 3.0");
        check(Double.doubleToRawLongBits(StrictMath.sin(OPAQUE_SM[2])) == 0x3feaed548f090ceeL,
                "StrictMath.sin(1.0) must be 0x3feaed548f090cee");
        check(Double.doubleToRawLongBits(StrictMath.cos(OPAQUE_SM[2])) == 0x3fe14a280fb5068cL,
                "StrictMath.cos(1.0) must be 0x3fe14a280fb5068c");
        check(Double.doubleToRawLongBits(StrictMath.tan(OPAQUE_SM[2])) == 0x3ff8eb245cbee3a6L,
                "StrictMath.tan(1.0) must be 0x3ff8eb245cbee3a6");
        check(Double.doubleToRawLongBits(StrictMath.atan2(OPAQUE_SM[2], OPAQUE_SM[0]))
                        == 0x3fddac670561bb4fL,
                "StrictMath.atan2(1.0, 2.0) must be 0x3fddac670561bb4f");
        check(Double.doubleToRawLongBits(StrictMath.hypot(OPAQUE_SM[3], OPAQUE_SM[10]))
                        == 0x4014000000000000L,
                "StrictMath.hypot(3.0, 4.0) must be EXACTLY 5.0 — no rounding error at all");
        check(Double.doubleToRawLongBits(StrictMath.expm1(OPAQUE_SM[11]))
                        == 0x3ddb7cdfd9dda4e3L,
                "StrictMath.expm1(1e-10) must be 0x3ddb7cdfd9dda4e3 — expm1 exists precisely so"
                        + " this is not exp(x)-1, which would round to 1e-10 exactly");
        check(Double.doubleToRawLongBits(StrictMath.log1p(OPAQUE_SM[11]))
                        == 0x3ddb7cdfd9d1d693L,
                "StrictMath.log1p(1e-10) must be 0x3ddb7cdfd9d1d693 — and must DIFFER from"
                        + " expm1's answer above in the last three hex digits");
        check(Double.doubleToRawLongBits(StrictMath.sinh(OPAQUE_SM[2])) == 0x3ff2cd9fc44eb982L,
                "StrictMath.sinh(1.0) must be 0x3ff2cd9fc44eb982");
        check(Double.doubleToRawLongBits(StrictMath.IEEEremainder(OPAQUE_SM[12], OPAQUE_SM[3]))
                        == 0xbff0000000000000L,
                "StrictMath.IEEEremainder(5.0, 3.0) must be -1.0 — it rounds the quotient to"
                        + " NEAREST, where the drem opcode truncates and answers 2.0");
        check(Double.doubleToRawLongBits(OPAQUE_SM[12] % OPAQUE_SM[3]) == 0x4000000000000000L,
                "and 5.0 % 3.0 must be 2.0 — the same operands, the other rounding rule");

        // GAP 2: Math must be within ONE ulp of StrictMath, everywhere in the
        // ordinary domain. Eight functions x ten operands = eighty comparisons
        // in one row; the 44-ulp shape fails it and a conforming variant does
        // not.
        double worst = 0;
        for (int k = 0; k < OPAQUE_SM.length; k++) {
            double x = OPAQUE_SM[k];
            if (!(x > 0.0)) {
                continue;
            }
            worst = Math.max(worst, ulpGap(Math.exp(x), StrictMath.exp(x)));
            worst = Math.max(worst, ulpGap(Math.log(x), StrictMath.log(x)));
            worst = Math.max(worst, ulpGap(Math.sin(x), StrictMath.sin(x)));
            worst = Math.max(worst, ulpGap(Math.cos(x), StrictMath.cos(x)));
            worst = Math.max(worst, ulpGap(Math.atan(x), StrictMath.atan(x)));
            worst = Math.max(worst, ulpGap(Math.cbrt(x), StrictMath.cbrt(x)));
            worst = Math.max(worst, ulpGap(Math.pow(x, OPAQUE_D[7]), StrictMath.pow(x, OPAQUE_D[7])));
            worst = Math.max(worst, ulpGap(Math.pow(OPAQUE_D[7], x), StrictMath.pow(OPAQUE_D[7], x)));
        }
        check(worst <= 1.0,
                "every Math transcendental must be within ONE ulp of its StrictMath twin over"
                        + " the whole ordinary operand set — Math's javadoc budget. Worst gap"
                        + " seen: " + worst + " ulp");
        check(Double.doubleToRawLongBits(Math.sqrt(OPAQUE_SM[0]))
                        == Double.doubleToRawLongBits(StrictMath.sqrt(OPAQUE_SM[0])),
                "Math.sqrt has NO error budget — it is required to be correctly rounded, so it"
                        + " must equal StrictMath.sqrt to the bit");

        // GAP 3: round / rint — two rounding rules on the same operands, and
        // the one input the JDK itself once got wrong.
        check(Math.round(OPAQUE_D[6]) == 1L, "Math.round(0.5) must be 1 — ties go UP");
        check(Math.round(OPAQUE_SM[14]) == 0L,
                "Math.round(-0.5) must be 0 — 'up' means toward POSITIVE infinity, so this is"
                        + " not symmetric with the row above");
        check(Math.round(OPAQUE_D[7]) == 3L, "Math.round(2.5) must be 3");
        check(Math.round(-OPAQUE_D[7]) == -2L, "Math.round(-2.5) must be -2");
        check(Math.round(OPAQUE_SM[13]) == 0L,
                "Math.round(0.49999999999999994) must be 0 — the largest double below 0.5."
                        + " The naive floor(x + 0.5) answers 1, and that WAS a JDK bug");
        check(Math.round(OPAQUE_D[2]) == 0L, "Math.round(NaN) must be 0");
        check(Math.round(Float.NaN) == 0, "Math.round(Float.NaN) must be 0");
        check(Math.round(Double.MAX_VALUE) == Long.MAX_VALUE,
                "Math.round(MAX_VALUE) must SATURATE to Long.MAX_VALUE");
        check(StrictMath.round(OPAQUE_D[7]) == 3L, "StrictMath.round(2.5) must agree: 3");
        check(StrictMath.round(OPAQUE_SM[13]) == 0L,
                "StrictMath.round(0.49999999999999994) must agree: 0");
        check(Double.doubleToRawLongBits(Math.rint(OPAQUE_D[7])) == 0x4000000000000000L,
                "Math.rint(2.5) must be 2.0 — HALF-EVEN, where Math.round(2.5) is 3 above."
                        + " Two rounding modes, one operand, and this is the pair that"
                        + " distinguishes them");
        check(Double.doubleToRawLongBits(Math.rint(OPAQUE_D[3])) == 0x4000000000000000L,
                "Math.rint(1.5) must ALSO be 2.0 — half-even rounds both 1.5 and 2.5 to 2");
        check(Double.doubleToRawLongBits(Math.rint(OPAQUE_SM[14])) == Long.MIN_VALUE,
                "Math.rint(-0.5) must be NEGATIVE zero");
        check(Double.doubleToRawLongBits(StrictMath.rint(OPAQUE_D[7])) == 0x4000000000000000L,
                "StrictMath.rint(2.5) must agree: 2.0");
        check(Double.doubleToRawLongBits(Math.ceil(OPAQUE_SM[14])) == Long.MIN_VALUE,
                "Math.ceil(-0.5) must be NEGATIVE zero, not positive zero");
        check(Double.doubleToRawLongBits(Math.floor(OPAQUE_SM[14])) == 0xbff0000000000000L,
                "Math.floor(-0.5) must be -1.0");

        // GAP 4: signum / copySign / nextUp / ulp — the sign-bit family.
        check(Double.doubleToRawLongBits(Math.signum(OPAQUE_D[0])) == Long.MIN_VALUE,
                "Math.signum(-0.0) must be -0.0, not 0.0 and not -1.0");
        check(Double.doubleToRawLongBits(Math.copySign(OPAQUE_SM[2], OPAQUE_D[0]))
                        == 0xbff0000000000000L,
                "Math.copySign(1.0, -0.0) must be -1.0 — the SIGN of a negative zero is"
                        + " readable, which == cannot see");
        check(Double.doubleToRawLongBits(StrictMath.copySign(OPAQUE_SM[2], OPAQUE_D[0]))
                        == 0xbff0000000000000L,
                "StrictMath.copySign must agree");
        check(Double.doubleToRawLongBits(Math.nextUp(OPAQUE_D[1])) == 1L,
                "Math.nextUp(0.0) must be Double.MIN_VALUE — the smallest subnormal");
        check(Double.doubleToRawLongBits(Math.nextDown(OPAQUE_D[1])) == 0x8000000000000001L,
                "Math.nextDown(0.0) must be -Double.MIN_VALUE");
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_SM[2])) == 0x3cb0000000000000L,
                "Math.ulp(1.0) must be 2^-52");
        check(Double.doubleToRawLongBits(Math.ulp(OPAQUE_D[1])) == 1L,
                "Math.ulp(0.0) must be MIN_VALUE, not zero");
        check(Double.doubleToRawLongBits(Math.abs(OPAQUE_D[2])) == 0x7ff8000000000000L,
                "Math.abs(NaN) must be the canonical NaN, not a sign-stripped payload");

        // GAP 5: the EXACT twins of the wrapping rows directly above, plus the
        // remaining integral statics. absExact is the complement of the four
        // step-guarded abs rows: same operand, opposite contract.
        step("strictExact", "Math.absExact(Integer.MIN_VALUE)");
        Throwable e = null;
        try {
            sink = Math.absExact(OPAQUE_I[0]);
        } catch (Throwable x) {
            e = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(e)),
                "Math.absExact(Integer.MIN_VALUE) must THROW ArithmeticException — the exact"
                        + " operand on which Math.abs WRAPS four rows above; got " + nameOf(e));
        step("strictExact", "Math.absExact(Long.MIN_VALUE)");
        e = null;
        try {
            sink = (int) Math.absExact(OPAQUE_J[0]);
        } catch (Throwable x) {
            e = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(e)),
                "Math.absExact(Long.MIN_VALUE) must throw ArithmeticException, got " + nameOf(e));
        check(Math.absExact(-5) == 5, "Math.absExact(-5) must simply be 5");
        check(Math.divideExact(7, OPAQUE_I[5]) == 3,
                "Math.divideExact(7, 2) must be 3 — it truncates like idiv");
        step("strictExact", "Math.divideExact(Integer.MIN_VALUE, -1)");
        e = null;
        try {
            sink = Math.divideExact(OPAQUE_I[0], OPAQUE_I[2]);
        } catch (Throwable x) {
            e = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(e)),
                "Math.divideExact(MIN_INT, -1) must throw ArithmeticException — where the idiv"
                        + " OPCODE wraps; got " + nameOf(e));
        check(Math.multiplyFull(OPAQUE_I[0], OPAQUE_I[0]) == 4611686018427387904L,
                "Math.multiplyFull(MIN_INT, MIN_INT) must be 2^62 — a long result that the"
                        + " (II)I product cannot hold");
        check(StrictMath.multiplyFull(OPAQUE_I[2], OPAQUE_I[2]) == 1L,
                "StrictMath.multiplyFull(-1, -1) must be 1, not 4294967295");
        check(Math.clamp(5L, OPAQUE_J[4], 3L) == 3L, "Math.clamp(5, 1, 3) must be 3");
        check(Double.doubleToRawLongBits(Math.clamp(OPAQUE_D[0], OPAQUE_D[0], OPAQUE_D[1]))
                        == Long.MIN_VALUE,
                "Math.clamp(-0.0, -0.0, 0.0) must be NEGATIVE zero — clamp orders the two"
                        + " zeros, so it cannot be written with a plain <=");
        e = null;
        try {
            sink = (int) Math.clamp(5L, 3L, OPAQUE_J[4]);
        } catch (Throwable x) {
            e = x;
        }
        check("java.lang.IllegalArgumentException".equals(nameOf(e)),
                "Math.clamp with min > max must throw IllegalArgumentException — NOT"
                        + " ArithmeticException, which every other row in this block raises;"
                        + " got " + nameOf(e));
        check(Math.toIntExact(OPAQUE_J[5]) == 2,
                "Math.toIntExact must exist on Math too, not only on StrictMath");
        e = null;
        try {
            sink = Math.toIntExact(OPAQUE_J[1]);
        } catch (Throwable x) {
            e = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(e)),
                "Math.toIntExact(MAX_LONG) must throw ArithmeticException, got " + nameOf(e));

        sectionEnd("strictExact", 91);
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

        // ===================================================================
        // E26 — the reach audit. Same hazard, wider.
        //
        // REACH BEFORE: StrictMath.floorDiv/floorMod, the ldiv and lrem
        // OPCODES, and one widened Math.floorMod. Never reached: Math's own
        // floorDiv/floorMod (separate registered triples from StrictMath's),
        // the INT opcodes at the same overflow point, the divide-by-zero
        // opcodes, the whole ceil family, and the four UNSIGNED division
        // methods — where the divisor being "negative" is the normal case.
        //
        // VALUE-DOMAIN NOTE: the rows above drive (-7, 2) and the MIN/-1
        // overflow. The plain TRUNCATING operators on the same operands were
        // never asked, so nothing here has ever shown that floorDiv and idiv
        // differ at all.
        // ===================================================================

        // GAP 1: the truncating operators next to the flooring methods, on the
        // SAME operands. Four answers, and no two of them agree.
        check(OPAQUE_I[9] / OPAQUE_I[5] == -3,
                "the idiv OPCODE must TRUNCATE -7/2 toward zero: -3");
        check(Math.floorDiv(OPAQUE_I[9], OPAQUE_I[5]) == -4,
                "Math.floorDiv(-7, 2) must FLOOR to -4 — the same operands, one apart");
        check(OPAQUE_I[9] % OPAQUE_I[5] == -1,
                "the irem OPCODE's sign follows the DIVIDEND: -7 % 2 is -1");
        check(Math.floorMod(OPAQUE_I[9], OPAQUE_I[5]) == 1,
                "Math.floorMod's sign follows the DIVISOR: floorMod(-7, 2) is 1");
        check(Math.floorDiv(OPAQUE_J[6], OPAQUE_I[5]) == -4L,
                "Math.floorDiv(-7L, 2) — the (JI)J overload, a third registered triple beside"
                        + " (II)I and (JJ)J");
        check(Math.floorMod(OPAQUE_J[6], OPAQUE_I[5]) == 1L,
                "Math.floorMod(-7L, 2) must be 1");

        // GAP 2: the INT opcodes at the overflow point the long ones were
        // checked at, and division by zero. Same JVMS 6.5 rule, separate
        // implementations.
        step("divmod", "idiv Integer.MIN_VALUE / -1");
        check(OPAQUE_I[0] / OPAQUE_I[2] == Integer.MIN_VALUE,
                "the idiv OPCODE must wrap MIN_INT / -1 to MIN_INT (JVMS 6.5), exactly as ldiv"
                        + " does above");
        step("divmod", "irem Integer.MIN_VALUE % -1");
        check(OPAQUE_I[0] % OPAQUE_I[2] == 0, "the irem OPCODE must answer 0 for MIN_INT % -1");
        step("divmod", "Math.floorDiv(Integer.MIN_VALUE, -1)");
        check(Math.floorDiv(OPAQUE_I[0], OPAQUE_I[2]) == Integer.MIN_VALUE,
                "Math.floorDiv(MIN_INT, -1) must wrap — StrictMath's twin is checked above and"
                        + " [2twins] is a standing record about exactly this pair");
        step("divmod", "idiv by zero");
        Throwable d = null;
        try {
            sink = OPAQUE_I[4] / OPAQUE_I[3];
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "1 / 0 must throw ArithmeticException from the OPCODE, got " + nameOf(d));
        d = null;
        try {
            sink = OPAQUE_I[4] % OPAQUE_I[3];
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "1 % 0 must throw ArithmeticException, got " + nameOf(d));
        d = null;
        try {
            sink = Math.floorDiv(OPAQUE_I[4], OPAQUE_I[3]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Math.floorDiv(1, 0) must throw ArithmeticException, got " + nameOf(d));

        // GAP 3: the UNSIGNED division family. Every operand here is negative
        // as a signed int and enormous as an unsigned one, which is the whole
        // point — a body that forwards to a signed divide gets all four wrong.
        step("divmod", "Integer.divideUnsigned(-1, 2)");
        check(Integer.divideUnsigned(OPAQUE_I[2], OPAQUE_I[5]) == Integer.MAX_VALUE,
                "Integer.divideUnsigned(-1, 2) must be 2147483647 — the SIGNED -1/2 is 0");
        check(Integer.remainderUnsigned(OPAQUE_I[2], OPAQUE_I[6]) == 0,
                "Integer.remainderUnsigned(-1, 3) must be 0 — 4294967295 is divisible by 3,"
                        + " where the signed -1 % 3 is -1");
        check(Integer.divideUnsigned(OPAQUE_I[2], -2) == 1,
                "Integer.divideUnsigned(-1, -2) must be 1 — BOTH operands read unsigned");
        check(Long.divideUnsigned(OPAQUE_J[2], OPAQUE_J[5]) == Long.MAX_VALUE,
                "Long.divideUnsigned(-1L, 2L) must be Long.MAX_VALUE");
        check(Long.remainderUnsigned(OPAQUE_J[2], 3L) == 0L,
                "Long.remainderUnsigned(-1L, 3L) must be 0");
        step("divmod", "Integer.divideUnsigned(1, 0)");
        d = null;
        try {
            sink = Integer.divideUnsigned(OPAQUE_I[4], OPAQUE_I[3]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Integer.divideUnsigned(1, 0) must throw ArithmeticException — Rust's"
                        + " u32 division PANICS here; got " + nameOf(d));
        d = null;
        try {
            sink = Integer.remainderUnsigned(OPAQUE_I[4], OPAQUE_I[3]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Integer.remainderUnsigned(1, 0) must throw ArithmeticException, got "
                        + nameOf(d));

        // GAP 4: the ceil family — a THIRD rounding direction, and the one
        // whose mod is signed opposite to floorMod's.
        check(Math.ceilDiv(OPAQUE_I[9], OPAQUE_I[5]) == -3,
                "Math.ceilDiv(-7, 2) must be -3 — rounds toward POSITIVE infinity, which here"
                        + " coincides with idiv and differs from floorDiv");
        check(Math.ceilDiv(7, OPAQUE_I[5]) == 4,
                "Math.ceilDiv(7, 2) must be 4 — and HERE it differs from idiv's 3, so the two"
                        + " rows together show it is neither");
        check(Math.ceilMod(OPAQUE_I[9], OPAQUE_I[5]) == -1, "Math.ceilMod(-7, 2) must be -1");
        check(Math.ceilMod(7, OPAQUE_I[5]) == -1,
                "Math.ceilMod(7, 2) must ALSO be -1 — ceilMod's sign follows the NEGATED"
                        + " divisor, so a positive dividend gives a negative remainder");
        check(Math.ceilDiv(OPAQUE_J[6], OPAQUE_J[5]) == -3L, "Math.ceilDiv(-7L, 2L) must be -3");
        step("divmod", "Math.ceilDiv(Integer.MIN_VALUE, -1)");
        check(Math.ceilDiv(OPAQUE_I[0], OPAQUE_I[2]) == Integer.MIN_VALUE,
                "Math.ceilDiv(MIN_INT, -1) must WRAP like its floor twin");
        step("divmod", "Math.ceilDivExact(Integer.MIN_VALUE, -1)");
        d = null;
        try {
            sink = Math.ceilDivExact(OPAQUE_I[0], OPAQUE_I[2]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Math.ceilDivExact(MIN_INT, -1) must THROW where ceilDiv wraps one row up; got "
                        + nameOf(d));
        step("divmod", "Math.floorDivExact(Integer.MIN_VALUE, -1)");
        d = null;
        try {
            sink = Math.floorDivExact(OPAQUE_I[0], OPAQUE_I[2]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Math.floorDivExact(MIN_INT, -1) must throw ArithmeticException, got "
                        + nameOf(d));
        d = null;
        try {
            sink = Math.ceilDiv(OPAQUE_I[4], OPAQUE_I[3]);
        } catch (Throwable x) {
            d = x;
        }
        check("java.lang.ArithmeticException".equals(nameOf(d)),
                "Math.ceilDiv(1, 0) must throw ArithmeticException, got " + nameOf(d));

        sectionEnd("divmod", 40);
    }

    // Ordered by how likely each family is to ABORT the VM rather than fail an
    // assertion, ascending. A Rust panic truncates the run, so anything after
    // the first aborting family never reports.
    static final String[] FAMILIES = {
        "charcls", "boolparse", "floatfmt", "hex", "b64", "uuid", "random", "strfmt",
        "bounds", "strnull", "strictExact", "divmod",
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
        } else if ("strnull".equals(name)) {
            strnull();
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
