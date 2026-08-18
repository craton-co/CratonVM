import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.PrintStream;
import java.lang.reflect.Field;
import java.io.File;
import java.io.StringReader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;
import java.text.DateFormat;
import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Base64;
import java.util.Date;
import java.util.EnumMap;
import java.util.Formatter;
import java.util.Iterator;
import java.util.List;
import java.util.Locale;
import java.util.Objects;
import java.util.Scanner;
import java.util.TimeZone;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.SimpleFormatter;
import java.util.logging.StreamHandler;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * THIRD-generation {@code NativeKind::Intrinsic} census: the 420 registered triples that neither
 * committed vector drives.
 *
 * <h2>The coverage arithmetic this file exists to close</h2>
 *
 * <p>Recomputed from the schema-4 {@code --dump-native-registry} dump used by
 * W8-C3-1, so the unit is the same throughout:
 *
 * <pre>
 *   registry rows with kind == "intrinsic"      645
 *   distinct (class, name, descriptor) triples  614   (31 rows are duplicate registrations)
 *   driven by RJdkIntrinsics  (generation 1)     36
 *   driven by RJdkIntrinsics2 (generation 2)    173
 *   union of generations 1 and 2                194   = 32%
 *   driven by NEITHER                           420
 * </pre>
 *
 * <p>This file drives <b>292 of those 420</b>, taking the union to <b>486 of 614 = 79%</b>. What is
 * left, and why each piece was left, is in
 * docs/known-issues/jdk-only/W8-C3-1-intrinsic-census-round-2.md and in this lane's record.
 *
 * <h2>The risk model — why these triples, in this order</h2>
 *
 * <p>Same five hazards W8-C3-1 established, and the family order below is that list read backwards,
 * so a VM that dies in {@link #mathexact()} has already reported the fifteen families before it.
 *
 * <ol>
 *   <li><b>Integer division, remainder and overflow.</b> Rust checks division overflow
 *       UNCONDITIONALLY — release as well as debug — and a Rust panic is not a Java throwable: it
 *       terminates the VM rather than unwinding to a {@code catch}. Java's rule is that
 *       {@code Math.addExact} and friends THROW {@link ArithmeticException}, and that the
 *       {@code idiv} opcode WRAPS. See {@link #mathexact()} and the {@code setScale} rows of
 *       {@link #bigdec()}.
 *   <li><b>Array indexing and slicing.</b> Rust panics on an out-of-bounds index; Java throws, and
 *       throws a SPECIFIC class. Every such row here asserts the exact class name, never
 *       {@code instanceof}, because a body that throws the generic superclass everywhere is wrong
 *       in a way an {@code instanceof} test cannot see. See {@link #bufslice()} and the
 *       {@code group(int)} rows of {@link #regex()}.
 *   <li><b>Anything taking a {@code String}.</b> A Rust {@code str} cannot hold an unpaired UTF-16
 *       surrogate and Rust's parsers implement Rust's grammars. See the {@code toString} rows
 *       throughout {@link #boxid()}, {@link #bigdec()} and {@link #bigint()}.
 *   <li><b>Float and double special values.</b> NaN, +-0.0, +-Infinity, MAX_VALUE, MIN_VALUE,
 *       subnormals. See {@link #strictd()}, {@link #mathd()}, {@link #boxconv()}.
 *   <li><b>A Rust standard-library equivalent whose edge semantics differ.</b> See
 *       {@link #objects()}, {@link #boxid()}, {@link #bitops()}.
 * </ol>
 *
 * <h2>Do not test only the corners</h2>
 *
 * <p>W7-95 sampled special values across {@code java.lang.Math} and reported the family correct;
 * {@code Math.pow}'s fast path was nonetheless <b>44 ulp wrong on ORDINARY inputs</b>. A
 * special-value census answers a different question from an interior census, so every
 * transcendental in {@link #strictd()} and {@link #mathd()} is driven at BOTH: the specified
 * special values, and a spread of ordinary arguments in the interior of the domain.
 *
 * <h2>How this file compares</h2>
 *
 * <p>Floating-point results go through {@link #ckD} / {@link #ckF}, which compare
 * {@link Double#doubleToRawLongBits} / {@link Float#floatToRawIntBits} — never {@code ==}, because
 * {@code -0.0 == 0.0} is {@code true} and {@code NaN != NaN}, so an equality-shaped check passes
 * against exactly the defects this file hunts.
 *
 * <p>{@code java.lang.Math}'s transcendentals are specified only to within 1 ulp (2 for
 * {@code pow} and {@code atan2}), so asserting their exact bits would be asserting more than the
 * spec says. Those rows use {@link #ckU} / {@link #ckUF}, which compare the bit patterns as a
 * total order and report the ULP DISTANCE — tight enough to catch the 44-ulp defect above, loose
 * enough that a conforming implementation cannot be failed for conforming.
 * {@code java.lang.StrictMath} is bit-exact by contract, so {@link #strictd()} uses {@link #ckD}
 * throughout and is the sharper of the two families.
 *
 * <p>Operands are non-final statics rather than literals or {@code static final} constants: a
 * {@code static final double} initialised with a literal is a compile-time constant and
 * {@code javac} folds every use of it, so the vector would be asserting against {@code javac}'s
 * arithmetic rather than the VM's native.
 *
 * <h2>Every expected value was MEASURED, none remembered</h2>
 *
 * <p>{@code --measure} prints every observable this file asserts, as a Java literal, on a
 * {@code CK}-prefixed line. The committed expectations were produced by running exactly that mode
 * on Microsoft OpenJDK 25.0.3+9 and substituting its output; no number in this file was typed from
 * memory. Re-derive them on any JDK with:
 *
 * <pre>
 *   java -cp regression-suite/build RJdkIntrinsics3 --measure
 * </pre>
 *
 * <h2>How it is driven</h2>
 *
 * <p>With no arguments — which is how {@code run.sh} drives it — all sixteen families run in
 * ascending order of how likely each is to ABORT the VM rather than fail an assertion.
 * {@code --only=<family>} runs one family alone, which is the only way to learn anything about a
 * family whose predecessor kills the process; {@code --list} prints the names. The two families
 * whose hazard is a panic rather than a wrong answer print a
 * {@code CK RJdkIntrinsics3 <family>-step=<call>} line BEFORE each risky call, so on a VM that
 * aborts, the last line on stdout names the call that killed it.
 *
 * <h2>Mode independence</h2>
 *
 * <p>These are language and class-library semantics, not a mode policy, and the natives are
 * registered in both arms — so this belongs in {@code CORE_CLASSES}, exactly like
 * {@code RJdkIntrinsics} and {@code RJdkIntrinsics2}.
 */
public class RJdkIntrinsics3 {
    static int checks;

    static int mark;

    /** {@code --measure}: print what this JDK answers instead of asserting against a stored value. */
    static boolean measuring;

    // Sinks. A call whose result is dropped can be elided; a call whose result reaches a static
    // field cannot.
    static int sinkI;
    static long sinkJ;
    static double sinkD;
    static float sinkF;
    static char sinkC;
    static boolean sinkZ;
    static Object sinkO;


    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Close a block and assert its own size. The count is a tripwire: a block that silently loses
     * rows to an edit still prints {@code CK}, and a hard-coded number nobody re-derives is how a
     * shrinking vector goes unnoticed. Mismatch is a failure, not a warning.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (measuring) {
            System.out.println("CK RJdkIntrinsics3 M|" + name + "|" + n);
            return;
        }
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RJdkIntrinsics3 " + name + "=" + n);
    }

    /**
     * A progress marker printed BEFORE a call that may abort the VM instead of throwing. On a VM
     * that panics, the last of these on stdout names the call that killed it; on a correct VM the
     * sequence is deterministic, so it diffs clean against the oracle.
     */
    static void step(String family, String what) {
        System.out.println("CK RJdkIntrinsics3 " + family + "-step=" + what);
    }

    /** The class of the throwable {@code op} produced, or {@code "none"}. */
    static String nameOf(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    static void emit(String label, String literal) {
        System.out.println("CK RJdkIntrinsics3 M|" + label + "|" + literal);
    }

    /** A Java source literal for {@code s}, escaped so a lone surrogate survives the round trip. */
    static String lit(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder b = new StringBuilder("\"");
        for (int k = 0; k < s.length(); k++) {
            char c = s.charAt(k);
            if (c == '"' || c == '\\') {
                b.append('\\').append(c);
            } else if (c == '\n') {
                b.append("\\n");
            } else if (c == '\r') {
                b.append("\\r");
            } else if (c == '\t') {
                b.append("\\t");
            } else if (c < 0x20 || c > 0x7e) {
                String h = Integer.toHexString(c);
                b.append("\\u");
                for (int p = h.length(); p < 4; p++) {
                    b.append('0');
                }
                b.append(h);
            } else {
                b.append(c);
            }
        }
        return b.append('"').toString();
    }

    // -- the comparators -----------------------------------------------------

    /** Exact double: RAW bits, so -0.0 and NaN payloads are distinguishable. */
    static void ckD(String w, double a, long e) {
        long b = Double.doubleToRawLongBits(a);
        checks++;
        if (measuring) {
            emit(w, "0x" + Long.toHexString(b) + "L");
            return;
        }
        if (b != e) {
            throw new AssertionError(w + ": expected raw bits 0x" + Long.toHexString(e) + " ("
                    + Double.longBitsToDouble(e) + "), got 0x" + Long.toHexString(b) + " (" + a
                    + ")");
        }
    }

    /** Exact float: RAW bits. */
    static void ckF(String w, float a, int e) {
        int b = Float.floatToRawIntBits(a);
        checks++;
        if (measuring) {
            emit(w, "0x" + Integer.toHexString(b));
            return;
        }
        if (b != e) {
            throw new AssertionError(w + ": expected raw bits 0x" + Integer.toHexString(e) + " ("
                    + Float.intBitsToFloat(e) + "), got 0x" + Integer.toHexString(b) + " (" + a
                    + ")");
        }
    }

    /** The bit pattern as a total order, so a difference in it is a difference in ulps. */
    static long ord(long bits) {
        return bits < 0 ? Long.MIN_VALUE - bits : bits;
    }

    static int ordF(int bits) {
        return bits < 0 ? Integer.MIN_VALUE - bits : bits;
    }

    /**
     * Double within {@code maxUlp}. For the {@code java.lang.Math} transcendentals ONLY, whose
     * javadoc specifies 1 ulp (2 for {@code pow} and {@code atan2}) rather than a bit pattern.
     * A special-value class mismatch — NaN vs finite, infinite vs finite — is never within
     * tolerance, because those ARE exactly specified.
     */
    static void ckU(String w, double a, long e, int maxUlp) {
        long b = Double.doubleToRawLongBits(a);
        checks++;
        if (measuring) {
            emit(w, "0x" + Long.toHexString(b) + "L");
            return;
        }
        if (b == e) {
            return;
        }
        double ev = Double.longBitsToDouble(e);
        if (Double.isNaN(a) != Double.isNaN(ev) || Double.isInfinite(a) != Double.isInfinite(ev)) {
            throw new AssertionError(w + ": expected " + ev + " (0x" + Long.toHexString(e)
                    + "), got " + a + " (0x" + Long.toHexString(b)
                    + ") -- the SPECIAL-VALUE CLASS differs, which is exactly specified; no ulp"
                    + " tolerance applies");
        }
        if (Double.isNaN(a)) {
            return;
        }
        long d = ord(b) - ord(e);
        if (d < 0) {
            d = -d;
        }
        if (d < 0 || d > maxUlp) {
            throw new AssertionError(w + ": expected " + ev + " (0x" + Long.toHexString(e)
                    + "), got " + a + " (0x" + Long.toHexString(b) + ") -- " + d
                    + " ulp apart, tolerance is " + maxUlp);
        }
    }

    static void ckUF(String w, float a, int e, int maxUlp) {
        int b = Float.floatToRawIntBits(a);
        checks++;
        if (measuring) {
            emit(w, "0x" + Integer.toHexString(b));
            return;
        }
        if (b == e) {
            return;
        }
        float ev = Float.intBitsToFloat(e);
        if (Float.isNaN(a) != Float.isNaN(ev) || Float.isInfinite(a) != Float.isInfinite(ev)) {
            throw new AssertionError(w + ": expected " + ev + ", got " + a
                    + " -- the SPECIAL-VALUE CLASS differs; no ulp tolerance applies");
        }
        if (Float.isNaN(a)) {
            return;
        }
        int d = ordF(b) - ordF(e);
        if (d < 0) {
            d = -d;
        }
        if (d < 0 || d > maxUlp) {
            throw new AssertionError(w + ": expected " + ev + ", got " + a + " -- " + d
                    + " ulp apart, tolerance is " + maxUlp);
        }
    }

    static void ckI(String w, int a, int e) {
        checks++;
        if (measuring) {
            emit(w, Integer.toString(a));
            return;
        }
        if (a != e) {
            throw new AssertionError(w + ": expected " + e + ", got " + a);
        }
    }

    static void ckJ(String w, long a, long e) {
        checks++;
        if (measuring) {
            emit(w, a + "L");
            return;
        }
        if (a != e) {
            throw new AssertionError(w + ": expected " + e + ", got " + a);
        }
    }

    static void ckB(String w, boolean a, boolean e) {
        checks++;
        if (measuring) {
            emit(w, Boolean.toString(a));
            return;
        }
        if (a != e) {
            throw new AssertionError(w + ": expected " + e + ", got " + a);
        }
    }

    static void ckS(String w, String a, String e) {
        checks++;
        if (measuring) {
            emit(w, lit(a));
            return;
        }
        if (a == null ? e != null : !a.equals(e)) {
            throw new AssertionError(w + ": expected " + lit(e) + ", got " + lit(a));
        }
    }

    /**
     * The EXACT class of the throwable a call produced, or {@code "none"}. Never
     * {@code instanceof}: a body that throws the generic superclass for every bounds failure is
     * wrong in a way an {@code instanceof} test cannot see. ({@code [subcls!=cls]}.)
     */
    static void ckX(String w, Throwable t, String e) {
        String got = nameOf(t);
        checks++;
        if (measuring) {
            emit(w, lit(got));
            return;
        }
        if (!e.equals(got)) {
            throw new AssertionError(w + ": expected " + e + ", got " + got
                    + (t == null ? " (the call RETURNED)" : ": " + t.getMessage()));
        }
    }

    // -- operand pools -------------------------------------------------------
    //
    // Deliberately NOT `static final`: a `static final double` initialised with a literal is a
    // compile-time constant, javac folds every read of it, and the vector would then be asserting
    // against javac's arithmetic instead of the VM's native.

    static double dZero = 0.0;
    static double dNegZero = -0.0;
    static double dOne = 1.0;
    static double dNegOne = -1.0;
    static double dHalf = 0.5;
    static double dNegHalf = -0.5;
    static double dTwo = 2.0;
    static double dThree = 3.0;
    static double dFour = 4.0;
    static double dFive = 5.0;
    static double dEight = 8.0;
    static double dNegEight = -8.0;
    static double dTen = 10.0;
    static double dHundred = 100.0;
    static double dThousand = 1000.0;
    static double d1p5 = 1.5;
    static double dNeg1p5 = -1.5;
    static double d2p5 = 2.5;
    static double dNeg2p5 = -2.5;
    static double d3p5 = 3.5;
    static double dNeg3p5 = -3.5;
    static double d0p1 = 0.1;
    static double d0p7 = 0.7;
    static double d0p25 = 0.25;
    static double dNaN = Double.NaN;
    static double dNegNaN = Double.longBitsToDouble(0xfff8000000000000L);
    static double dInf = Double.POSITIVE_INFINITY;
    static double dNegInf = Double.NEGATIVE_INFINITY;
    static double dMax = Double.MAX_VALUE;
    static double dMin = Double.MIN_VALUE;
    static double dMinNormal = Double.MIN_NORMAL;
    static double dPi = Math.PI;
    static double dE = Math.E;
    static double d1e300 = 1e300;
    static double d1eNeg300 = 1e-300;
    static double d1e17 = 1e17;
    static double d1e7 = 1e7;
    static double dBig = 123456.789;
    static double dJustAbove1 = 1.0000000000000002;
    /** The input that made {@code floor(x + 0.5)} wrong; {@code Math.round} must answer 0. */
    static double dJustBelowHalf = 0.49999999999999994;

    static float fZero = 0.0f;
    static float fNegZero = -0.0f;
    static float fOne = 1.0f;
    static float fNegOne = -1.0f;
    static float fTwo = 2.0f;
    static float fHalf = 0.5f;
    static float fNegHalf = -0.5f;
    static float f2p5 = 2.5f;
    static float fNeg2p5 = -2.5f;
    static float f3p5 = 3.5f;
    static float f1p1 = 1.1f;
    static float fNaN = Float.NaN;
    static float fNegNaN = Float.intBitsToFloat(0xffc00000);
    static float fInf = Float.POSITIVE_INFINITY;
    static float fNegInf = Float.NEGATIVE_INFINITY;
    static float fMax = Float.MAX_VALUE;
    static float fMin = Float.MIN_VALUE;
    /** The float analogue of {@link #dJustBelowHalf}. */
    static float fJustBelowHalf = 0.49999997f;

    static int iMin = Integer.MIN_VALUE;
    static int iMax = Integer.MAX_VALUE;
    static int iZero = 0;
    static int iOne = 1;
    static int iNegOne = -1;
    static int iTwo = 2;
    static int iThree = 3;
    static int iFive = 5;
    static int iNegFive = -5;
    static int i255 = 255;
    static int i65536 = 65536;
    static int iPat = 0x12345678;

    static long jMin = Long.MIN_VALUE;
    static long jMax = Long.MAX_VALUE;
    static long jZero = 0L;
    static long jOne = 1L;
    static long jNegOne = -1L;
    static long jFive = 5L;
    static long j2p31 = 2147483648L;
    static long jNeg2p31m1 = -2147483649L;
    static long jPat = 0x123456789abcdefL;

    // ========================================================================
    // 1. objects — java.util.Objects, 11 triples.
    //
    // W7-95 measured this family and reported it agreeing, but it is in the 420
    // because NO COMMITTED VECTOR drives it, and "a probe agreed once" is not a
    // guard. Every row here is a null-handling contract, which is precisely
    // where a Rust body reaches for `Option` and answers a different question:
    // `Objects.hash()` is 1 and `Objects.hash((Object[]) null)` is 0, two
    // answers a single `unwrap_or_default()` cannot both give.
    // ========================================================================
    static void objects() {
        Object a = new Object();
        String s1 = new String(new char[] {'a', 'b', 'c'});
        String s2 = new String(new char[] {'a', 'b', 'c'});

        ckB("objects:equals(null,null)", Objects.equals(null, null), true);
        ckB("objects:equals(null,\"abc\")", Objects.equals(null, s1), false);
        ckB("objects:equals(\"abc\",null)", Objects.equals(s1, null), false);
        ckB("objects:equals(distinct equal Strings)", Objects.equals(s1, s2), true);
        ckB("objects:equals(a,a)", Objects.equals(a, a), true);

        ckI("objects:hashCode(null)", Objects.hashCode(null), 0);
        ckI("objects:hashCode(\"abc\")", Objects.hashCode(s1), 96354);

        // The three arities of hash() that a single defaulting body cannot all satisfy.
        ckI("objects:hash()", Objects.hash(), 1);
        ckI("objects:hash((Object[]) null)", Objects.hash((Object[]) null), 0);
        ckI("objects:hash(\"a\",\"b\")", Objects.hash("a", "b"), 4066);
        ckI("objects:hash(null-element)", Objects.hash(new Object[] {null}), 31);
        ckB("objects:hash(x,y) == Arrays.hashCode",
                Objects.hash("a", "b") == Arrays.hashCode(new Object[] {"a", "b"}), true);

        ckB("objects:isNull(null)", Objects.isNull(null), true);
        ckB("objects:isNull(a)", Objects.isNull(a), false);
        ckB("objects:nonNull(null)", Objects.nonNull(null), false);
        ckB("objects:nonNull(a)", Objects.nonNull(a), true);

        ckS("objects:toString(null)", Objects.toString(null), "null");
        ckS("objects:toString(\"abc\")", Objects.toString(s1), "abc");
        ckS("objects:toString(null,\"dflt\")", Objects.toString(null, "dflt"), "dflt");
        ckS("objects:toString(\"abc\",\"dflt\")", Objects.toString(s1, "dflt"), "abc");

        // requireNonNull returns its argument BY IDENTITY, and throws with the caller's message
        // verbatim rather than a message of its own.
        check(Objects.requireNonNull(s1) == s1,
                "objects: requireNonNull must return the SAME reference, not a copy");
        Throwable t = null;
        try {
            sinkO = Objects.requireNonNull(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("objects:requireNonNull(null) throws", t, "java.lang.NullPointerException");
        ckS("objects:requireNonNull(null) message", t == null ? "?" : t.getMessage(), null);
        t = null;
        try {
            sinkO = Objects.requireNonNull(null, "my message");
        } catch (Throwable x) {
            t = x;
        }
        ckX("objects:requireNonNull(null,msg) throws", t, "java.lang.NullPointerException");
        ckS("objects:requireNonNull(null,msg) message", t == null ? "?" : t.getMessage(), "my message");

        ckS("objects:requireNonNullElse(null,\"b\")",
                (String) Objects.requireNonNullElse(null, "b"), "b");
        ckS("objects:requireNonNullElse(\"a\",\"b\")", Objects.requireNonNullElse(s1, "b"), "abc");
        t = null;
        try {
            sinkO = Objects.requireNonNullElse(null, null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("objects:requireNonNullElse(null,null) throws", t, "java.lang.NullPointerException");

        ckS("objects:requireNonNullElseGet(null,supplier)",
                (String) Objects.requireNonNullElseGet(null, new Sup("supplied")), "supplied");
        // The supplier must NOT be consulted when the value is present, which is observable
        // because a null supplier would otherwise throw.
        ckS("objects:requireNonNullElseGet(\"a\",null supplier)",
                Objects.requireNonNullElseGet(s1, null), "abc");
        t = null;
        try {
            sinkO = Objects.requireNonNullElseGet(null, new Sup(null));
        } catch (Throwable x) {
            t = x;
        }
        ckX("objects:requireNonNullElseGet(null,()->null) throws", t, "java.lang.NullPointerException");

        sectionEnd("objects", 31);
    }

    // ========================================================================
    // 2. boxid — the box IDENTITY, equality, hash and toString surface.
    //
    // 28 triples across Integer/Long/Byte/Short/Character/Double/Float/Boolean.
    //
    // Two mechanisms, neither of which a value-comparing Rust body can get
    // right:
    //
    //   * valueOf is a CACHE with a specified range. Integer.valueOf(127) and
    //     Integer.valueOf(127) must be the SAME OBJECT; Integer.valueOf(128)
    //     and Integer.valueOf(128) must NOT be. The rows below use reference
    //     `==` deliberately — that is the property under test.
    //   * equals is TYPE-SENSITIVE. Integer.valueOf(5).equals(Long.valueOf(5))
    //     is false. A body that compares numeric values answers true.
    // ========================================================================
    static void boxid() {
        // -- the valueOf caches, at both ends of each specified range ---------
        ckB("boxid:Integer.valueOf(127) identity",
                Integer.valueOf(i255 - 128) == Integer.valueOf(i255 - 128), true);
        ckB("boxid:Integer.valueOf(-128) identity",
                Integer.valueOf(-128) == Integer.valueOf(-128), true);
        ckB("boxid:Integer.valueOf(128) identity",
                Integer.valueOf(i255 - 127) == Integer.valueOf(i255 - 127), false);
        ckB("boxid:Integer.valueOf(-129) identity",
                Integer.valueOf(-129) == Integer.valueOf(-129), false);
        ckI("boxid:Integer.valueOf(MIN).intValue", Integer.valueOf(iMin).intValue(), -2147483648);

        ckB("boxid:Long.valueOf(127) identity", Long.valueOf(127L) == Long.valueOf(127L), true);
        ckB("boxid:Long.valueOf(128) identity", Long.valueOf(128L) == Long.valueOf(128L), false);
        ckJ("boxid:Long.valueOf(MIN).longValue", Long.valueOf(jMin).longValue(), -9223372036854775808L);

        // Byte's cache covers the WHOLE type, so every Byte.valueOf is interned.
        ckB("boxid:Byte.valueOf(-128) identity",
                Byte.valueOf((byte) -128) == Byte.valueOf((byte) -128), true);
        ckB("boxid:Byte.valueOf(127) identity",
                Byte.valueOf((byte) 127) == Byte.valueOf((byte) 127), true);

        ckB("boxid:Short.valueOf(-128) identity",
                Short.valueOf((short) -128) == Short.valueOf((short) -128), true);
        ckB("boxid:Short.valueOf(128) identity",
                Short.valueOf((short) 128) == Short.valueOf((short) 128), false);

        ckB("boxid:Character.valueOf(127) identity",
                Character.valueOf((char) 127) == Character.valueOf((char) 127), true);
        ckB("boxid:Character.valueOf(128) identity",
                Character.valueOf((char) 128) == Character.valueOf((char) 128), false);

        ckB("boxid:Boolean.valueOf(true) is Boolean.TRUE", Boolean.valueOf(true) == Boolean.TRUE,
                true);
        ckB("boxid:Boolean.valueOf(false) is Boolean.FALSE",
                Boolean.valueOf(false) == Boolean.FALSE, true);

        // -- equals is type-sensitive ----------------------------------------
        ckB("boxid:Integer(5).equals(Long(5))", Integer.valueOf(iFive).equals(Long.valueOf(jFive)),
                false);
        ckB("boxid:Long(5).equals(Integer(5))", Long.valueOf(jFive).equals(Integer.valueOf(iFive)),
                false);
        ckB("boxid:Byte(5).equals(Integer(5))",
                Byte.valueOf((byte) 5).equals(Integer.valueOf(iFive)), false);
        ckB("boxid:Short(5).equals(Integer(5))",
                Short.valueOf((short) 5).equals(Integer.valueOf(iFive)), false);
        ckB("boxid:Character('a').equals(Integer(97))",
                Character.valueOf('a').equals(Integer.valueOf(97)), false);
        ckB("boxid:Boolean(true).equals(\"true\")", Boolean.valueOf(true).equals("true"), false);
        ckB("boxid:Integer(5).equals(Integer(5))",
                Integer.valueOf(iFive).equals(Integer.valueOf(iFive)), true);
        ckB("boxid:Integer(MIN).equals(Integer(MIN))",
                Integer.valueOf(iMin).equals(Integer.valueOf(iMin)), true);
        ckB("boxid:Long(MIN).equals(Long(MIN))", Long.valueOf(jMin).equals(Long.valueOf(jMin)), true);
        ckB("boxid:Byte(-1).equals(Byte(-1))",
                Byte.valueOf((byte) -1).equals(Byte.valueOf((byte) -1)), true);
        ckB("boxid:Short(-1).equals(Short(-1))",
                Short.valueOf((short) -1).equals(Short.valueOf((short) -1)), true);

        // -- hashCode is a SPECIFIED function, not an identity hash ----------
        ckI("boxid:Integer(MIN).hashCode", Integer.valueOf(iMin).hashCode(), -2147483648);
        ckI("boxid:Integer(-1).hashCode", Integer.valueOf(iNegOne).hashCode(), -1);
        // Long.hashCode is (int)(v ^ (v >>> 32)) -- an UNSIGNED shift, so a body using an
        // arithmetic shift gets every negative long wrong.
        ckI("boxid:Long(MIN).hashCode", Long.valueOf(jMin).hashCode(), -2147483648);
        ckI("boxid:Long(-1).hashCode", Long.valueOf(jNegOne).hashCode(), 0);
        ckI("boxid:Long(0x123456789abcdef).hashCode", Long.valueOf(jPat).hashCode(), -2004318072);
        ckI("boxid:Byte(-1).hashCode", Byte.valueOf((byte) -1).hashCode(), -1);
        ckI("boxid:Short(-1).hashCode", Short.valueOf((short) -1).hashCode(), -1);
        ckI("boxid:Character(0xffff).hashCode", Character.valueOf((char) 0xffff).hashCode(), 65535);
        // Float.hashCode is floatToIntBits, which CANONICALISES NaN and keeps -0.0's sign bit.
        ckI("boxid:Float(0.0f).hashCode", Float.valueOf(fZero).hashCode(), 0);
        ckI("boxid:Float(-0.0f).hashCode", Float.valueOf(fNegZero).hashCode(), -2147483648);
        ckI("boxid:Float(NaN).hashCode", Float.valueOf(fNaN).hashCode(), 2143289344);
        ckI("boxid:Float(1.1f).hashCode", Float.valueOf(f1p1).hashCode(), 1066192077);
        ckB("boxid:Float(0.0f).equals(Float(-0.0f))",
                Float.valueOf(fZero).equals(Float.valueOf(fNegZero)), false);
        ckB("boxid:Float(NaN).equals(Float(NaN))",
                Float.valueOf(fNaN).equals(Float.valueOf(fNaN)), true);

        // -- toString: shortest-round-trip, which Rust's Display is NOT -------
        ckS("boxid:Integer(MIN).toString", Integer.valueOf(iMin).toString(), "-2147483648");
        ckS("boxid:Long(MIN).toString", Long.valueOf(jMin).toString(), "-9223372036854775808");
        ckS("boxid:Byte(-128).toString", Byte.valueOf((byte) -128).toString(), "-128");
        ckS("boxid:Short(-32768).toString", Short.valueOf((short) -32768).toString(), "-32768");
        ckS("boxid:Boolean(true).toString", Boolean.valueOf(true).toString(), "true");
        ckS("boxid:Double(1.0).toString", Double.valueOf(dOne).toString(), "1.0");
        ckS("boxid:Double(-0.0).toString", Double.valueOf(dNegZero).toString(), "-0.0");
        ckS("boxid:Double(1e7).toString", Double.valueOf(d1e7).toString(), "1.0E7");
        ckS("boxid:Double(0.1).toString", Double.valueOf(d0p1).toString(), "0.1");
        ckS("boxid:Double(NaN).toString", Double.valueOf(dNaN).toString(), "NaN");
        ckS("boxid:Double(MIN_VALUE).toString", Double.valueOf(dMin).toString(), "4.9E-324");
        ckS("boxid:Float(1.0f).toString", Float.valueOf(fOne).toString(), "1.0");
        ckS("boxid:Float(1.1f).toString", Float.valueOf(f1p1).toString(), "1.1");
        ckS("boxid:Float(-Inf).toString", Float.valueOf(fNegInf).toString(), "-Infinity");
        ckS("boxid:Character('a').toString", Character.valueOf('a').toString(), "a");

        // A LONE SURROGATE as a one-character String. A Rust `str` cannot hold this, so a body
        // that round-trips the char through `str` either substitutes U+FFFD or truncates.
        String lone = Character.valueOf((char) 0xd800).toString();
        ckI("boxid:Character(0xd800).toString().length", lone.length(), 1);
        ckI("boxid:Character(0xd800).toString().charAt(0)", lone.charAt(0), 55296);

        // -- the two Character predicates left over from generation 2 ---------
        // Java's isWhitespace EXCLUDES the non-breaking spaces (they are space chars but not
        // whitespace) and INCLUDES the C0 information separators. Rust's char::is_whitespace
        // says the opposite on both.
        ckB("boxid:Character.isWhitespace(0x00a0 NBSP)", Character.isWhitespace(0x00a0), false);
        ckB("boxid:Character.isWhitespace(0x2007 FIGURE SPACE)", Character.isWhitespace(0x2007),
                false);
        ckB("boxid:Character.isWhitespace(0x001c FILE SEPARATOR)", Character.isWhitespace(0x001c),
                true);
        ckB("boxid:Character.isWhitespace(0x0085 NEL)", Character.isWhitespace(0x0085), false);
        ckB("boxid:Character.isWhitespace(0x2028 LINE SEP)", Character.isWhitespace(0x2028), true);
        ckB("boxid:Character.isWhitespace(0x0020)", Character.isWhitespace(0x0020), true);
        ckB("boxid:Character.isWhitespace(-1)", Character.isWhitespace(iNegOne), false);
        ckB("boxid:Character.isLetterOrDigit('a')", Character.isLetterOrDigit('a'), true);
        ckB("boxid:Character.isLetterOrDigit(0x2160 ROMAN ONE)",
                Character.isLetterOrDigit((char) 0x2160), false);
        ckB("boxid:Character.isLetterOrDigit(0x00b2 SUPERSCRIPT TWO)",
                Character.isLetterOrDigit((char) 0x00b2), false);
        ckB("boxid:Character.isLetterOrDigit('_')", Character.isLetterOrDigit('_'), false);

        sectionEnd("boxid", 69);
    }

    // ========================================================================
    // 3. boxconv — the widening/narrowing conversions and compare, 29 triples.
    //
    // Hazard 4 (float special values) and hazard 5 (different edge semantics)
    // in the same block:
    //
    //   * d2i / d2l are SATURATING and map NaN to 0 (JVMS 6.5). Rust's `as`
    //     casts agree, but a body that reaches for `unwrap` or a checked cast
    //     does not.
    //   * Integer.compare(MIN, 1) must be -1. The naive `a - b` OVERFLOWS and
    //     yields a positive number, which is the classic wrong body.
    //   * Byte(-1).intValue() must SIGN-EXTEND to -1. A Rust `u8` does not.
    // ========================================================================
    static void boxconv() {
        // -- Integer -> wider, with the values that do not fit exactly --------
        ckD("boxconv:Integer(MAX).doubleValue", Integer.valueOf(iMax).doubleValue(), 0x41dfffffffc00000L);
        ckF("boxconv:Integer(MAX).floatValue", Integer.valueOf(iMax).floatValue(), 0x4f000000);
        ckF("boxconv:Integer(MIN).floatValue", Integer.valueOf(iMin).floatValue(), 0xcf000000);
        ckJ("boxconv:Integer(MIN).longValue", Integer.valueOf(iMin).longValue(), -2147483648L);

        // -- Long -> double/float, where MAX_VALUE rounds UP ------------------
        ckD("boxconv:Long(MAX).doubleValue", Long.valueOf(jMax).doubleValue(), 0x43e0000000000000L);
        ckF("boxconv:Long(MAX).floatValue", Long.valueOf(jMax).floatValue(), 0x5f000000);
        ckD("boxconv:Long(MIN).doubleValue", Long.valueOf(jMin).doubleValue(), 0xc3e0000000000000L);
        ckI("boxconv:Long(MAX).intValue", Long.valueOf(jMax).intValue(), -1);
        ckI("boxconv:Long(0x123456789abcdef).intValue", Long.valueOf(jPat).intValue(), -1985229329);

        // -- Byte/Short SIGN-EXTEND ------------------------------------------
        ckI("boxconv:Byte(-1).byteValue", Byte.valueOf((byte) -1).byteValue(), -1);
        ckI("boxconv:Byte(-1).intValue", Byte.valueOf((byte) -1).intValue(), -1);
        ckJ("boxconv:Byte(-1).longValue", Byte.valueOf((byte) -1).longValue(), -1L);
        ckD("boxconv:Byte(-128).doubleValue", Byte.valueOf((byte) -128).doubleValue(), 0xc060000000000000L);
        ckF("boxconv:Byte(-128).floatValue", Byte.valueOf((byte) -128).floatValue(), 0xc3000000);
        ckI("boxconv:Short(-1).shortValue", Short.valueOf((short) -1).shortValue(), -1);
        ckI("boxconv:Short(-32768).intValue", Short.valueOf((short) -32768).intValue(), -32768);
        ckJ("boxconv:Short(-32768).longValue", Short.valueOf((short) -32768).longValue(), -32768L);
        ckD("boxconv:Short(-32768).doubleValue", Short.valueOf((short) -32768).doubleValue(), 0xc0e0000000000000L);
        ckF("boxconv:Short(-32768).floatValue", Short.valueOf((short) -32768).floatValue(), 0xc7000000);

        ckI("boxconv:Character(0xffff).charValue", Character.valueOf((char) 0xffff).charValue(),
                65535);

        // -- Double -> narrower: SATURATING, NaN to zero ----------------------
        ckI("boxconv:Double(NaN).intValue", Double.valueOf(dNaN).intValue(), 0);
        ckI("boxconv:Double(+Inf).intValue", Double.valueOf(dInf).intValue(), 2147483647);
        ckI("boxconv:Double(-Inf).intValue", Double.valueOf(dNegInf).intValue(), -2147483648);
        ckI("boxconv:Double(1e300).intValue", Double.valueOf(d1e300).intValue(), 2147483647);
        ckI("boxconv:Double(-2.5).intValue", Double.valueOf(dNeg2p5).intValue(), -2);
        ckJ("boxconv:Double(NaN).longValue", Double.valueOf(dNaN).longValue(), 0L);
        ckJ("boxconv:Double(+Inf).longValue", Double.valueOf(dInf).longValue(), 9223372036854775807L);
        ckJ("boxconv:Double(-Inf).longValue", Double.valueOf(dNegInf).longValue(), -9223372036854775808L);
        ckJ("boxconv:Double(1e17).longValue", Double.valueOf(d1e17).longValue(), 100000000000000000L);
        ckF("boxconv:Double(1e300).floatValue", Double.valueOf(d1e300).floatValue(), 0x7f800000);
        ckF("boxconv:Double(MIN_VALUE).floatValue", Double.valueOf(dMin).floatValue(), 0x0);
        ckF("boxconv:Double(-0.0).floatValue", Double.valueOf(dNegZero).floatValue(), 0x80000000);
        ckF("boxconv:Double(NaN).floatValue", Double.valueOf(dNaN).floatValue(), 0x7fc00000);

        ckB("boxconv:Double.isNaN(NaN)", Double.isNaN(dNaN), true);
        ckB("boxconv:Double.isNaN(+Inf)", Double.isNaN(dInf), false);
        ckB("boxconv:Double.isNaN(0.0)", Double.isNaN(dZero), false);

        // -- Float -> wider/narrower ------------------------------------------
        ckD("boxconv:Float(1.1f).doubleValue", Float.valueOf(f1p1).doubleValue(), 0x3ff19999a0000000L);
        ckD("boxconv:Float(NaN).doubleValue", Float.valueOf(fNaN).doubleValue(), 0x7ff8000000000000L);
        ckD("boxconv:Float(-0.0f).doubleValue", Float.valueOf(fNegZero).doubleValue(), 0x8000000000000000L);
        ckI("boxconv:Float(NaN).intValue", Float.valueOf(fNaN).intValue(), 0);
        ckI("boxconv:Float(MAX).intValue", Float.valueOf(fMax).intValue(), 2147483647);
        ckJ("boxconv:Float(-Inf).longValue", Float.valueOf(fNegInf).longValue(), -9223372036854775808L);
        ckJ("boxconv:Float(MAX).longValue", Float.valueOf(fMax).longValue(), 9223372036854775807L);

        // -- compare: the subtraction that overflows --------------------------
        ckI("boxconv:Integer.compare(MIN,1)", Integer.compare(iMin, iOne), -1);
        ckI("boxconv:Integer.compare(MAX,-1)", Integer.compare(iMax, iNegOne), 1);
        ckI("boxconv:Integer.compare(MIN,MAX)", Integer.compare(iMin, iMax), -1);
        ckI("boxconv:Integer.compare(5,5)", Integer.compare(iFive, iFive), 0);
        ckI("boxconv:Long.compare(MIN,1)", Long.compare(jMin, jOne), -1);
        ckI("boxconv:Long.compare(MAX,-1)", Long.compare(jMax, jNegOne), 1);
        ckI("boxconv:Long.compare(0,0)", Long.compare(jZero, jZero), 0);
        ckI("boxconv:Integer(MIN).compareTo(Integer(MAX))",
                Integer.valueOf(iMin).compareTo(Integer.valueOf(iMax)), -1);
        ckI("boxconv:Long(MIN).compareTo(Long(MAX))",
                Long.valueOf(jMin).compareTo(Long.valueOf(jMax)), -1);

        // Double.compare is a TOTAL ORDER: -0.0 sorts below 0.0 and NaN sorts above everything,
        // which is exactly what `==` and a naive `partial_cmp` cannot express.
        ckI("boxconv:Double.compare(-0.0,0.0)", Double.compare(dNegZero, dZero), -1);
        ckI("boxconv:Double.compare(0.0,-0.0)", Double.compare(dZero, dNegZero), 1);
        ckI("boxconv:Double.compare(NaN,NaN)", Double.compare(dNaN, dNaN), 0);
        ckI("boxconv:Double.compare(NaN,+Inf)", Double.compare(dNaN, dInf), 1);
        ckI("boxconv:Double.compare(+Inf,NaN)", Double.compare(dInf, dNaN), -1);
        ckI("boxconv:Double.compare(-0.0,-0.0)", Double.compare(dNegZero, dNegZero), 0);

        sectionEnd("boxconv", 58);
    }

    // ========================================================================
    // 4. bitops — Integer/Long bit manipulation, 10 triples.
    //
    // Rust's u32::leading_zeros agrees with Java on 0 (both 32), which is the
    // one edge everybody checks. The rows that separate the implementations are
    // the SIGNED inputs — Java's argument is an `int`, so a body that widens to
    // u64 before counting answers 32 too many — and `reverse`, which is a bit
    // reversal and not a byte reversal.
    // ========================================================================
    static void bitops() {
        ckI("bitops:Integer.bitCount(0)", Integer.bitCount(iZero), 0);
        ckI("bitops:Integer.bitCount(-1)", Integer.bitCount(iNegOne), 32);
        ckI("bitops:Integer.bitCount(MIN)", Integer.bitCount(iMin), 1);
        ckI("bitops:Integer.bitCount(0x12345678)", Integer.bitCount(iPat), 13);
        ckI("bitops:Integer.numberOfLeadingZeros(0)", Integer.numberOfLeadingZeros(iZero), 32);
        ckI("bitops:Integer.numberOfLeadingZeros(1)", Integer.numberOfLeadingZeros(iOne), 31);
        ckI("bitops:Integer.numberOfLeadingZeros(-1)", Integer.numberOfLeadingZeros(iNegOne), 0);
        ckI("bitops:Integer.numberOfLeadingZeros(MIN)", Integer.numberOfLeadingZeros(iMin), 0);
        ckI("bitops:Integer.numberOfTrailingZeros(0)", Integer.numberOfTrailingZeros(iZero), 32);
        ckI("bitops:Integer.numberOfTrailingZeros(1)", Integer.numberOfTrailingZeros(iOne), 0);
        ckI("bitops:Integer.numberOfTrailingZeros(MIN)", Integer.numberOfTrailingZeros(iMin), 31);
        ckI("bitops:Integer.reverse(1)", Integer.reverse(iOne), -2147483648);
        ckI("bitops:Integer.reverse(-1)", Integer.reverse(iNegOne), -1);
        ckI("bitops:Integer.reverse(0x12345678)", Integer.reverse(iPat), 510274632);
        ckI("bitops:Integer.reverseBytes(0x12345678)", Integer.reverseBytes(iPat), 2018915346);
        ckI("bitops:Integer.reverseBytes(-1)", Integer.reverseBytes(iNegOne), -1);
        ckB("bitops:Integer.reverse is an involution",
                Integer.reverse(Integer.reverse(iPat)) == iPat, true);

        ckI("bitops:Long.bitCount(0)", Long.bitCount(jZero), 0);
        ckI("bitops:Long.bitCount(-1)", Long.bitCount(jNegOne), 64);
        ckI("bitops:Long.bitCount(MIN)", Long.bitCount(jMin), 1);
        ckI("bitops:Long.bitCount(0x123456789abcdef)", Long.bitCount(jPat), 32);
        ckI("bitops:Long.numberOfLeadingZeros(0)", Long.numberOfLeadingZeros(jZero), 64);
        ckI("bitops:Long.numberOfLeadingZeros(1)", Long.numberOfLeadingZeros(jOne), 63);
        ckI("bitops:Long.numberOfLeadingZeros(-1)", Long.numberOfLeadingZeros(jNegOne), 0);
        ckI("bitops:Long.numberOfLeadingZeros(0x123456789abcdef)",
                Long.numberOfLeadingZeros(jPat), 7);
        ckI("bitops:Long.numberOfTrailingZeros(0)", Long.numberOfTrailingZeros(jZero), 64);
        ckI("bitops:Long.numberOfTrailingZeros(MIN)", Long.numberOfTrailingZeros(jMin), 63);
        ckJ("bitops:Long.reverse(1)", Long.reverse(jOne), -9223372036854775808L);
        ckJ("bitops:Long.reverse(-1)", Long.reverse(jNegOne), -1L);
        ckJ("bitops:Long.reverse(0x123456789abcdef)", Long.reverse(jPat), -597899502893742976L);
        ckJ("bitops:Long.reverseBytes(0x123456789abcdef)", Long.reverseBytes(jPat), -1167088121787636991L);
        ckJ("bitops:Long.reverseBytes(-1)", Long.reverseBytes(jNegOne), -1L);
        ckB("bitops:Long.reverse is an involution", Long.reverse(Long.reverse(jPat)) == jPat, true);

        sectionEnd("bitops", 33);
    }

    // ========================================================================
    // 5. strictd — java.lang.StrictMath's 39 double/float triples.
    //
    // The sharpest family in the file, because StrictMath is BIT-EXACT BY
    // CONTRACT: its results are defined to be those of the fdlibm algorithms,
    // so every row is an exact raw-bit assertion and no tolerance applies. A
    // body that forwards StrictMath to Rust's f64 intrinsics — which are the
    // platform libm, not fdlibm — differs in the last place on ordinary
    // arguments, which is what these rows are for.
    //
    // Each function is driven at BOTH ends of the risk: the specified special
    // values (NaN, +-0.0, +-Inf, MAX, MIN, subnormal) and a spread of ORDINARY
    // arguments in the interior of the domain. Corners alone are not a census:
    // Math.pow's fast path was 44 ulp wrong on ordinary inputs while every one
    // of its special values was correct.
    // ========================================================================
    static void strictd() {
        // -- rint: HALF-EVEN, which Rust's f64::round (half-away-from-zero) is not.
        ckD("strictd:rint(2.5)", StrictMath.rint(d2p5), 0x4000000000000000L);
        ckD("strictd:rint(3.5)", StrictMath.rint(d3p5), 0x4010000000000000L);
        ckD("strictd:rint(-2.5)", StrictMath.rint(dNeg2p5), 0xc000000000000000L);
        ckD("strictd:rint(-3.5)", StrictMath.rint(dNeg3p5), 0xc010000000000000L);
        ckD("strictd:rint(0.5)", StrictMath.rint(dHalf), 0x0L);
        ckD("strictd:rint(-0.5)", StrictMath.rint(dNegHalf), 0x8000000000000000L);
        ckD("strictd:rint(-0.0)", StrictMath.rint(dNegZero), 0x8000000000000000L);
        ckD("strictd:rint(NaN)", StrictMath.rint(dNaN), 0x7ff8000000000000L);
        ckD("strictd:rint(+Inf)", StrictMath.rint(dInf), 0x7ff0000000000000L);
        ckD("strictd:rint(123456.789)", StrictMath.rint(dBig), 0x40fe241000000000L);

        // -- round: floor(x + 0.5) with the 0.49999999999999994 correction.
        ckJ("strictd:round(0.5)", StrictMath.round(dHalf), 1L);
        ckJ("strictd:round(-0.5)", StrictMath.round(dNegHalf), 0L);
        ckJ("strictd:round(2.5)", StrictMath.round(d2p5), 3L);
        ckJ("strictd:round(-2.5)", StrictMath.round(dNeg2p5), -2L);
        ckJ("strictd:round(0.49999999999999994)", StrictMath.round(dJustBelowHalf), 0L);
        ckJ("strictd:round(NaN)", StrictMath.round(dNaN), 0L);
        ckJ("strictd:round(+Inf)", StrictMath.round(dInf), 9223372036854775807L);
        ckJ("strictd:round(-Inf)", StrictMath.round(dNegInf), -9223372036854775808L);
        ckJ("strictd:round(MAX_VALUE)", StrictMath.round(dMax), 9223372036854775807L);
        ckI("strictd:round(0.5f)", StrictMath.round(fHalf), 1);
        ckI("strictd:round(-0.5f)", StrictMath.round(fNegHalf), 0);
        ckI("strictd:round(2.5f)", StrictMath.round(f2p5), 3);
        ckI("strictd:round(-2.5f)", StrictMath.round(fNeg2p5), -2);
        ckI("strictd:round(0.49999997f)", StrictMath.round(fJustBelowHalf), 0);
        ckI("strictd:round(NaN f)", StrictMath.round(fNaN), 0);
        ckI("strictd:round(MAX f)", StrictMath.round(fMax), 2147483647);

        // -- ceil/floor: the NEGATIVE ZERO results an int-returning body loses.
        ckD("strictd:ceil(-0.5)", StrictMath.ceil(dNegHalf), 0x8000000000000000L);
        ckD("strictd:ceil(-0.0)", StrictMath.ceil(dNegZero), 0x8000000000000000L);
        ckD("strictd:ceil(0.0)", StrictMath.ceil(dZero), 0x0L);
        ckD("strictd:ceil(2.5)", StrictMath.ceil(d2p5), 0x4008000000000000L);
        ckD("strictd:ceil(NaN)", StrictMath.ceil(dNaN), 0x7ff8000000000000L);
        ckD("strictd:ceil(+Inf)", StrictMath.ceil(dInf), 0x7ff0000000000000L);
        ckD("strictd:ceil(MAX_VALUE)", StrictMath.ceil(dMax), 0x7fefffffffffffffL);
        ckD("strictd:floor(-0.0)", StrictMath.floor(dNegZero), 0x8000000000000000L);
        ckD("strictd:floor(-2.5)", StrictMath.floor(dNeg2p5), 0xc008000000000000L);
        ckD("strictd:floor(0.5)", StrictMath.floor(dHalf), 0x0L);
        ckD("strictd:floor(NaN)", StrictMath.floor(dNaN), 0x7ff8000000000000L);
        ckD("strictd:floor(-Inf)", StrictMath.floor(dNegInf), 0xfff0000000000000L);

        // -- signum: -0.0 in, -0.0 out.
        ckD("strictd:signum(-0.0)", StrictMath.signum(dNegZero), 0x8000000000000000L);
        ckD("strictd:signum(0.0)", StrictMath.signum(dZero), 0x0L);
        ckD("strictd:signum(-2.5)", StrictMath.signum(dNeg2p5), 0xbff0000000000000L);
        ckD("strictd:signum(NaN)", StrictMath.signum(dNaN), 0x7ff8000000000000L);
        ckD("strictd:signum(-Inf)", StrictMath.signum(dNegInf), 0xbff0000000000000L);
        ckF("strictd:signum(-0.0f)", StrictMath.signum(fNegZero), 0x80000000);
        ckF("strictd:signum(NaN f)", StrictMath.signum(fNaN), 0x7fc00000);
        ckF("strictd:signum(-2.5f)", StrictMath.signum(fNeg2p5), 0xbf800000);

        // -- copySign. StrictMath.copySign REQUIRES a NaN sign argument to be treated as
        // POSITIVE, so copySign(1.0, -NaN) is +1.0 and not -1.0 -- the opposite of the raw
        // sign bit, and the opposite of what a body forwarding to Rust's f64::copysign
        // computes. Math.copySign is explicitly RELIEVED of that requirement (its javadoc
        // permits either answer for performance), which is why mathd() asserts only the
        // magnitude on the same input and this block asserts the value. Two registered twins,
        // one of which has a licence the other does not.
        //
        // MEASURED, not remembered: an earlier draft of this comment asserted the raw sign
        // bit and the --measure run said 0x3ff0000000000000 (+1.0).
        ckD("strictd:copySign(2.5,-0.0)", StrictMath.copySign(d2p5, dNegZero), 0xc004000000000000L);
        ckD("strictd:copySign(-2.5,0.0)", StrictMath.copySign(dNeg2p5, dZero), 0x4004000000000000L);
        ckD("strictd:copySign(1.0,-NaN)", StrictMath.copySign(dOne, dNegNaN), 0x3ff0000000000000L);
        ckD("strictd:copySign(1.0,NaN)", StrictMath.copySign(dOne, dNaN), 0x3ff0000000000000L);
        ckD("strictd:copySign(NaN,-1.0)", StrictMath.copySign(dNaN, dNegOne), 0xfff8000000000000L);
        ckD("strictd:copySign(-Inf,1.0)", StrictMath.copySign(dNegInf, dOne), 0x7ff0000000000000L);
        ckF("strictd:copySign(2.5f,-0.0f)", StrictMath.copySign(f2p5, fNegZero), 0xc0200000);
        ckF("strictd:copySign(1.0f,-NaN f)", StrictMath.copySign(fOne, fNegNaN), 0x3f800000);

        // -- max/min: -0.0 is STRICTLY LESS THAN 0.0 here, and NaN wins both.
        ckD("strictd:max(-0.0,0.0)", StrictMath.max(dNegZero, dZero), 0x0L);
        ckD("strictd:max(0.0,-0.0)", StrictMath.max(dZero, dNegZero), 0x0L);
        ckD("strictd:min(-0.0,0.0)", StrictMath.min(dNegZero, dZero), 0x8000000000000000L);
        ckD("strictd:min(0.0,-0.0)", StrictMath.min(dZero, dNegZero), 0x8000000000000000L);
        ckD("strictd:max(NaN,1.0)", StrictMath.max(dNaN, dOne), 0x7ff8000000000000L);
        ckD("strictd:min(NaN,1.0)", StrictMath.min(dNaN, dOne), 0x7ff8000000000000L);
        ckD("strictd:max(1.0,2.5)", StrictMath.max(dOne, d2p5), 0x4004000000000000L);
        ckD("strictd:min(-Inf,MIN_VALUE)", StrictMath.min(dNegInf, dMin), 0xfff0000000000000L);
        ckF("strictd:max(-0.0f,0.0f)", StrictMath.max(fNegZero, fZero), 0x0);
        ckF("strictd:min(-0.0f,0.0f)", StrictMath.min(fNegZero, fZero), 0x80000000);
        ckF("strictd:max(NaN f,1.0f)", StrictMath.max(fNaN, fOne), 0x7fc00000);
        ckF("strictd:min(NaN f,1.0f)", StrictMath.min(fNaN, fOne), 0x7fc00000);

        // -- nextAfter / nextUp / nextDown: the subnormal boundary and the sign of zero.
        ckD("strictd:nextAfter(0.0,-1.0)", StrictMath.nextAfter(dZero, dNegOne), 0x8000000000000001L);
        ckD("strictd:nextAfter(0.0,1.0)", StrictMath.nextAfter(dZero, dOne), 0x1L);
        ckD("strictd:nextAfter(1.0,1.0)", StrictMath.nextAfter(dOne, dOne), 0x3ff0000000000000L);
        ckD("strictd:nextAfter(MAX,+Inf)", StrictMath.nextAfter(dMax, dInf), 0x7ff0000000000000L);
        ckD("strictd:nextAfter(1.0,+Inf)", StrictMath.nextAfter(dOne, dInf), 0x3ff0000000000001L);
        ckD("strictd:nextAfter(NaN,1.0)", StrictMath.nextAfter(dNaN, dOne), 0x7ff8000000000000L);
        ckD("strictd:nextUp(-0.0)", StrictMath.nextUp(dNegZero), 0x1L);
        ckD("strictd:nextUp(0.0)", StrictMath.nextUp(dZero), 0x1L);
        ckD("strictd:nextUp(-Inf)", StrictMath.nextUp(dNegInf), 0xffefffffffffffffL);
        ckD("strictd:nextUp(1.0)", StrictMath.nextUp(dOne), 0x3ff0000000000001L);
        ckD("strictd:nextDown(0.0)", StrictMath.nextDown(dZero), 0x8000000000000001L);
        ckD("strictd:nextDown(-0.0)", StrictMath.nextDown(dNegZero), 0x8000000000000001L);
        ckD("strictd:nextDown(+Inf)", StrictMath.nextDown(dInf), 0x7fefffffffffffffL);
        ckD("strictd:nextDown(1.0)", StrictMath.nextDown(dOne), 0x3fefffffffffffffL);

        // -- getExponent: defined for zero, subnormal and NaN, where "the exponent" is a
        // convention rather than a bit field.
        ckI("strictd:getExponent(0.0)", StrictMath.getExponent(dZero), -1023);
        ckI("strictd:getExponent(MIN_VALUE subnormal)", StrictMath.getExponent(dMin), -1023);
        ckI("strictd:getExponent(MIN_NORMAL)", StrictMath.getExponent(dMinNormal), -1022);
        ckI("strictd:getExponent(NaN)", StrictMath.getExponent(dNaN), 1024);
        ckI("strictd:getExponent(+Inf)", StrictMath.getExponent(dInf), 1024);
        ckI("strictd:getExponent(1.0)", StrictMath.getExponent(dOne), 0);
        ckI("strictd:getExponent(MAX_VALUE)", StrictMath.getExponent(dMax), 1023);

        // -- IEEEremainder: ROUND-HALF-EVEN remainder, which is NOT Java's % and NOT Rust's %.
        // IEEEremainder(5,3) is -1.0, where 5 % 3 is 2.0.
        ckD("strictd:IEEEremainder(5,3)", StrictMath.IEEEremainder(dFive, dThree), 0xbff0000000000000L);
        ckD("strictd:IEEEremainder(4,3)", StrictMath.IEEEremainder(dFour, dThree), 0x3ff0000000000000L);
        ckD("strictd:IEEEremainder(-5,3)", StrictMath.IEEEremainder(-dFive, dThree), 0x3ff0000000000000L);
        ckD("strictd:IEEEremainder(1.5,1.0)", StrictMath.IEEEremainder(d1p5, dOne), 0xbfe0000000000000L);
        ckD("strictd:IEEEremainder(2.5,1.0)", StrictMath.IEEEremainder(d2p5, dOne), 0x3fe0000000000000L);
        ckD("strictd:IEEEremainder(5,0)", StrictMath.IEEEremainder(dFive, dZero), 0xfff8000000000000L);
        ckD("strictd:IEEEremainder(+Inf,1)", StrictMath.IEEEremainder(dInf, dOne), 0xfff8000000000000L);
        ckD("strictd:IEEEremainder(1,+Inf)", StrictMath.IEEEremainder(dOne, dInf), 0x3ff0000000000000L);
        ckD("strictd:IEEEremainder(-0.0,1.0)", StrictMath.IEEEremainder(dNegZero, dOne), 0x8000000000000000L);

        // -- sqrt: exactly rounded, including the SIGNED zero and the negative-argument NaN.
        ckD("strictd:sqrt(-0.0)", StrictMath.sqrt(dNegZero), 0x8000000000000000L);
        ckD("strictd:sqrt(0.0)", StrictMath.sqrt(dZero), 0x0L);
        ckD("strictd:sqrt(-1.0)", StrictMath.sqrt(dNegOne), 0xfff8000000000000L);
        ckD("strictd:sqrt(NaN)", StrictMath.sqrt(dNaN), 0x7ff8000000000000L);
        ckD("strictd:sqrt(+Inf)", StrictMath.sqrt(dInf), 0x7ff0000000000000L);
        ckD("strictd:sqrt(2.0)", StrictMath.sqrt(dTwo), 0x3ff6a09e667f3bcdL);
        ckD("strictd:sqrt(MIN_VALUE)", StrictMath.sqrt(dMin), 0x1e60000000000000L);
        ckD("strictd:sqrt(MAX_VALUE)", StrictMath.sqrt(dMax), 0x5fefffffffffffffL);
        ckD("strictd:sqrt(0.1)", StrictMath.sqrt(d0p1), 0x3fd43d136248490fL);

        // -- cbrt: the ONLY root function that is defined on negatives.
        ckD("strictd:cbrt(-8.0)", StrictMath.cbrt(dNegEight), 0xc000000000000000L);
        ckD("strictd:cbrt(8.0)", StrictMath.cbrt(dEight), 0x4000000000000000L);
        ckD("strictd:cbrt(-0.0)", StrictMath.cbrt(dNegZero), 0x8000000000000000L);
        ckD("strictd:cbrt(NaN)", StrictMath.cbrt(dNaN), 0x7ff8000000000000L);
        ckD("strictd:cbrt(-Inf)", StrictMath.cbrt(dNegInf), 0xfff0000000000000L);
        ckD("strictd:cbrt(0.1)", StrictMath.cbrt(d0p1), 0x3fddb4c7760bcff3L);
        ckD("strictd:cbrt(123456.789)", StrictMath.cbrt(dBig), 0x4048e58dab7da808L);

        // -- hypot: must NOT overflow intermediately, and +-Inf BEATS NaN.
        ckD("strictd:hypot(3,4)", StrictMath.hypot(dThree, dFour), 0x4014000000000000L);
        ckD("strictd:hypot(+Inf,NaN)", StrictMath.hypot(dInf, dNaN), 0x7ff0000000000000L);
        ckD("strictd:hypot(NaN,-Inf)", StrictMath.hypot(dNaN, dNegInf), 0x7ff0000000000000L);
        ckD("strictd:hypot(NaN,1.0)", StrictMath.hypot(dNaN, dOne), 0x7ff8000000000000L);
        ckD("strictd:hypot(MAX,MAX)", StrictMath.hypot(dMax, dMax), 0x7ff0000000000000L);
        ckD("strictd:hypot(1e300,1e300)", StrictMath.hypot(d1e300, d1e300), 0x7e40e4d50f99b211L);
        ckD("strictd:hypot(-0.0,-0.0)", StrictMath.hypot(dNegZero, dNegZero), 0x0L);
        ckD("strictd:hypot(0.1,0.7)", StrictMath.hypot(d0p1, d0p7), 0x3fe6a09e667f3bccL);

        // -- pow: two ulp on Math, EXACT on StrictMath. The interior rows are the ones that
        // caught the 44-ulp fast path; the special values below are a different question.
        ckD("strictd:pow(2,10)", StrictMath.pow(dTwo, dTen), 0x4090000000000000L);
        ckD("strictd:pow(2,0.5)", StrictMath.pow(dTwo, dHalf), 0x3ff6a09e667f3bcdL);
        ckD("strictd:pow(0.1,3.0)", StrictMath.pow(d0p1, dThree), 0x3f50624dd2f1a9fdL);
        ckD("strictd:pow(3.0,0.7)", StrictMath.pow(dThree, d0p7), 0x400142e81c889914L);
        ckD("strictd:pow(1.0000000000000002,1e17)", StrictMath.pow(dJustAbove1, d1e17), 0x41f06272889f21c5L);
        ckD("strictd:pow(123456.789,0.25)", StrictMath.pow(dBig, d0p25), 0x4032bea55de63d4fL);
        ckD("strictd:pow(-2.0,3.0)", StrictMath.pow(-dTwo, dThree), 0xc020000000000000L);
        ckD("strictd:pow(-2.0,3.5)", StrictMath.pow(-dTwo, d3p5), 0xfff8000000000000L);
        ckD("strictd:pow(1.0,NaN)", StrictMath.pow(dOne, dNaN), 0x7ff8000000000000L);
        ckD("strictd:pow(NaN,0.0)", StrictMath.pow(dNaN, dZero), 0x3ff0000000000000L);
        ckD("strictd:pow(-1.0,+Inf)", StrictMath.pow(dNegOne, dInf), 0xfff8000000000000L);
        ckD("strictd:pow(0.0,-1.0)", StrictMath.pow(dZero, dNegOne), 0x7ff0000000000000L);
        ckD("strictd:pow(-0.0,-3.0)", StrictMath.pow(dNegZero, -dThree), 0xfff0000000000000L);
        ckD("strictd:pow(-0.0,3.0)", StrictMath.pow(dNegZero, dThree), 0x8000000000000000L);
        ckD("strictd:pow(1e300,2.0)", StrictMath.pow(d1e300, dTwo), 0x7ff0000000000000L);

        // -- exp / expm1 / log / log10 / log1p ---------------------------------
        ckD("strictd:exp(0.0)", StrictMath.exp(dZero), 0x3ff0000000000000L);
        ckD("strictd:exp(1.0)", StrictMath.exp(dOne), 0x4005bf0a8b14576aL);
        ckD("strictd:exp(-Inf)", StrictMath.exp(dNegInf), 0x0L);
        ckD("strictd:exp(+Inf)", StrictMath.exp(dInf), 0x7ff0000000000000L);
        ckD("strictd:exp(NaN)", StrictMath.exp(dNaN), 0x7ff8000000000000L);
        ckD("strictd:exp(1000.0)", StrictMath.exp(dThousand), 0x7ff0000000000000L);
        ckD("strictd:exp(0.1)", StrictMath.exp(d0p1), 0x3ff1aec7b35a00d4L);
        ckD("strictd:exp(-2.5)", StrictMath.exp(dNeg2p5), 0x3fb50385c094f425L);
        ckD("strictd:expm1(-0.0)", StrictMath.expm1(dNegZero), 0x8000000000000000L);
        ckD("strictd:expm1(-Inf)", StrictMath.expm1(dNegInf), 0xbff0000000000000L);
        ckD("strictd:expm1(NaN)", StrictMath.expm1(dNaN), 0x7ff8000000000000L);
        ckD("strictd:expm1(1e-300)", StrictMath.expm1(d1eNeg300), 0x1a56e1fc2f8f359L);
        ckD("strictd:expm1(0.1)", StrictMath.expm1(d0p1), 0x3fbaec7b35a00d3aL);
        ckD("strictd:expm1(1.0)", StrictMath.expm1(dOne), 0x3ffb7e151628aed2L);
        ckD("strictd:log(0.0)", StrictMath.log(dZero), 0xfff0000000000000L);
        ckD("strictd:log(-0.0)", StrictMath.log(dNegZero), 0xfff0000000000000L);
        ckD("strictd:log(-1.0)", StrictMath.log(dNegOne), 0xfff8000000000000L);
        ckD("strictd:log(1.0)", StrictMath.log(dOne), 0x0L);
        ckD("strictd:log(+Inf)", StrictMath.log(dInf), 0x7ff0000000000000L);
        ckD("strictd:log(E)", StrictMath.log(dE), 0x3ff0000000000000L);
        ckD("strictd:log(0.1)", StrictMath.log(d0p1), 0xc0026bb1bbb55515L);
        ckD("strictd:log(123456.789)", StrictMath.log(dBig), 0x40277281cad8a844L);
        ckD("strictd:log(MIN_VALUE)", StrictMath.log(dMin), 0xc0874385446d71c3L);
        ckD("strictd:log10(1000.0)", StrictMath.log10(dThousand), 0x4008000000000000L);
        ckD("strictd:log10(100.0)", StrictMath.log10(dHundred), 0x4000000000000000L);
        ckD("strictd:log10(0.0)", StrictMath.log10(dZero), 0xfff0000000000000L);
        ckD("strictd:log10(-1.0)", StrictMath.log10(dNegOne), 0xfff8000000000000L);
        ckD("strictd:log10(0.1)", StrictMath.log10(d0p1), 0xbff0000000000000L);
        ckD("strictd:log10(123456.789)", StrictMath.log10(dBig), 0x40145db61a282512L);
        ckD("strictd:log1p(-1.0)", StrictMath.log1p(dNegOne), 0xfff0000000000000L);
        ckD("strictd:log1p(-2.0)", StrictMath.log1p(-dTwo), 0x7ff8000000000000L);
        ckD("strictd:log1p(-0.0)", StrictMath.log1p(dNegZero), 0x8000000000000000L);
        ckD("strictd:log1p(+Inf)", StrictMath.log1p(dInf), 0x7ff0000000000000L);
        ckD("strictd:log1p(1e-300)", StrictMath.log1p(d1eNeg300), 0x1a56e1fc2f8f359L);
        ckD("strictd:log1p(0.1)", StrictMath.log1p(d0p1), 0x3fb8663f793c46c7L);

        // -- the circular functions and their inverses -------------------------
        ckD("strictd:sin(-0.0)", StrictMath.sin(dNegZero), 0x8000000000000000L);
        ckD("strictd:sin(+Inf)", StrictMath.sin(dInf), 0xfff8000000000000L);
        ckD("strictd:sin(NaN)", StrictMath.sin(dNaN), 0x7ff8000000000000L);
        ckD("strictd:sin(1.0)", StrictMath.sin(dOne), 0x3feaed548f090ceeL);
        ckD("strictd:sin(PI)", StrictMath.sin(dPi), 0x3ca1a62633145c07L);
        ckD("strictd:sin(0.1)", StrictMath.sin(d0p1), 0x3fb98eaecb8bcb2cL);
        ckD("strictd:sin(123456.789)", StrictMath.sin(dBig), 0xbfeff50e60ab53f9L);
        ckD("strictd:sin(1e17)", StrictMath.sin(d1e17), 0xbfddbadc7a119fc8L);
        ckD("strictd:cos(0.0)", StrictMath.cos(dZero), 0x3ff0000000000000L);
        ckD("strictd:cos(-Inf)", StrictMath.cos(dNegInf), 0xfff8000000000000L);
        ckD("strictd:cos(NaN)", StrictMath.cos(dNaN), 0x7ff8000000000000L);
        ckD("strictd:cos(1.0)", StrictMath.cos(dOne), 0x3fe14a280fb5068cL);
        ckD("strictd:cos(PI)", StrictMath.cos(dPi), 0xbff0000000000000L);
        ckD("strictd:cos(0.7)", StrictMath.cos(d0p7), 0x3fe87996529f9d93L);
        ckD("strictd:cos(123456.789)", StrictMath.cos(dBig), 0x3faa74d27c41b22aL);
        ckD("strictd:tan(-0.0)", StrictMath.tan(dNegZero), 0x8000000000000000L);
        ckD("strictd:tan(+Inf)", StrictMath.tan(dInf), 0xfff8000000000000L);
        ckD("strictd:tan(1.0)", StrictMath.tan(dOne), 0x3ff8eb245cbee3a6L);
        ckD("strictd:tan(PI)", StrictMath.tan(dPi), 0xbca1a62633145c07L);
        ckD("strictd:tan(0.1)", StrictMath.tan(d0p1), 0x3fb9af8877430b80L);
        ckD("strictd:tan(123456.789)", StrictMath.tan(dBig), 0xc03353a85fe8d6d5L);
        ckD("strictd:asin(-0.0)", StrictMath.asin(dNegZero), 0x8000000000000000L);
        ckD("strictd:asin(2.0)", StrictMath.asin(dTwo), 0xfff8000000000000L);
        ckD("strictd:asin(1.0)", StrictMath.asin(dOne), 0x3ff921fb54442d18L);
        ckD("strictd:asin(0.1)", StrictMath.asin(d0p1), 0x3fb9a49276037884L);
        ckD("strictd:asin(0.7)", StrictMath.asin(d0p7), 0x3fe8d00e692afd95L);
        ckD("strictd:acos(2.0)", StrictMath.acos(dTwo), 0xfff8000000000000L);
        ckD("strictd:acos(1.0)", StrictMath.acos(dOne), 0x0L);
        ckD("strictd:acos(-1.0)", StrictMath.acos(dNegOne), 0x400921fb54442d18L);
        ckD("strictd:acos(0.1)", StrictMath.acos(d0p1), 0x3ff787b22ce3f590L);
        ckD("strictd:acos(0.7)", StrictMath.acos(d0p7), 0x3fe973e83f5d5c9bL);
        ckD("strictd:atan(-0.0)", StrictMath.atan(dNegZero), 0x8000000000000000L);
        ckD("strictd:atan(+Inf)", StrictMath.atan(dInf), 0x3ff921fb54442d18L);
        ckD("strictd:atan(-Inf)", StrictMath.atan(dNegInf), 0xbff921fb54442d18L);
        ckD("strictd:atan(1.0)", StrictMath.atan(dOne), 0x3fe921fb54442d18L);
        ckD("strictd:atan(0.1)", StrictMath.atan(d0p1), 0x3fb983e282e2cc4dL);
        ckD("strictd:atan(123456.789)", StrictMath.atan(dBig), 0x3ff921f2d5f068d7L);
        // atan2's quadrant rules over signed zeros are the densest special-value table in
        // java.lang.Math, and every one of them is a DIFFERENT answer.
        ckD("strictd:atan2(0.0,-0.0)", StrictMath.atan2(dZero, dNegZero), 0x400921fb54442d18L);
        ckD("strictd:atan2(-0.0,-0.0)", StrictMath.atan2(dNegZero, dNegZero), 0xc00921fb54442d18L);
        ckD("strictd:atan2(0.0,0.0)", StrictMath.atan2(dZero, dZero), 0x0L);
        ckD("strictd:atan2(-0.0,1.0)", StrictMath.atan2(dNegZero, dOne), 0x8000000000000000L);
        ckD("strictd:atan2(-0.0,-1.0)", StrictMath.atan2(dNegZero, dNegOne), 0xc00921fb54442d18L);
        ckD("strictd:atan2(+Inf,+Inf)", StrictMath.atan2(dInf, dInf), 0x3fe921fb54442d18L);
        ckD("strictd:atan2(-Inf,-Inf)", StrictMath.atan2(dNegInf, dNegInf), 0xc002d97c7f3321d2L);
        ckD("strictd:atan2(1.0,NaN)", StrictMath.atan2(dOne, dNaN), 0x7ff8000000000000L);
        ckD("strictd:atan2(1.0,2.0)", StrictMath.atan2(dOne, dTwo), 0x3fddac670561bb4fL);
        ckD("strictd:atan2(-3.5,0.7)", StrictMath.atan2(dNeg3p5, d0p7), 0xbff5f97315254857L);

        // -- the hyperbolics ---------------------------------------------------
        ckD("strictd:sinh(-0.0)", StrictMath.sinh(dNegZero), 0x8000000000000000L);
        ckD("strictd:sinh(+Inf)", StrictMath.sinh(dInf), 0x7ff0000000000000L);
        ckD("strictd:sinh(NaN)", StrictMath.sinh(dNaN), 0x7ff8000000000000L);
        ckD("strictd:sinh(1.0)", StrictMath.sinh(dOne), 0x3ff2cd9fc44eb982L);
        ckD("strictd:sinh(0.1)", StrictMath.sinh(d0p1), 0x3fb9a487337b59b3L);
        ckD("strictd:sinh(1000.0)", StrictMath.sinh(dThousand), 0x7ff0000000000000L);
        ckD("strictd:cosh(0.0)", StrictMath.cosh(dZero), 0x3ff0000000000000L);
        ckD("strictd:cosh(-0.0)", StrictMath.cosh(dNegZero), 0x3ff0000000000000L);
        ckD("strictd:cosh(-Inf)", StrictMath.cosh(dNegInf), 0x7ff0000000000000L);
        ckD("strictd:cosh(1.0)", StrictMath.cosh(dOne), 0x3ff8b07551d9f551L);
        ckD("strictd:cosh(0.7)", StrictMath.cosh(d0p7), 0x3ff4152c1862342fL);
        ckD("strictd:cosh(1000.0)", StrictMath.cosh(dThousand), 0x7ff0000000000000L);
        ckD("strictd:tanh(-0.0)", StrictMath.tanh(dNegZero), 0x8000000000000000L);
        ckD("strictd:tanh(+Inf)", StrictMath.tanh(dInf), 0x3ff0000000000000L);
        ckD("strictd:tanh(-Inf)", StrictMath.tanh(dNegInf), 0xbff0000000000000L);
        ckD("strictd:tanh(1.0)", StrictMath.tanh(dOne), 0x3fe85efab514f394L);
        ckD("strictd:tanh(0.1)", StrictMath.tanh(d0p1), 0x3fb983d7795f413aL);
        ckD("strictd:tanh(1000.0)", StrictMath.tanh(dThousand), 0x3ff0000000000000L);

        // -- the degree/radian conversions -------------------------------------
        ckD("strictd:toDegrees(PI)", StrictMath.toDegrees(dPi), 0x4066800000000000L);
        ckD("strictd:toDegrees(-0.0)", StrictMath.toDegrees(dNegZero), 0x8000000000000000L);
        ckD("strictd:toDegrees(NaN)", StrictMath.toDegrees(dNaN), 0x7ff8000000000000L);
        ckD("strictd:toDegrees(+Inf)", StrictMath.toDegrees(dInf), 0x7ff0000000000000L);
        ckD("strictd:toDegrees(1.0)", StrictMath.toDegrees(dOne), 0x404ca5dc1a63c1f8L);
        ckD("strictd:toRadians(180.0)", StrictMath.toRadians(dHundred + 80.0), 0x400921fb54442d18L);
        ckD("strictd:toRadians(-0.0)", StrictMath.toRadians(dNegZero), 0x8000000000000000L);
        ckD("strictd:toRadians(NaN)", StrictMath.toRadians(dNaN), 0x7ff8000000000000L);
        ckD("strictd:toRadians(1.0)", StrictMath.toRadians(dOne), 0x3f91df46a2529d39L);
        ckD("strictd:toRadians(123456.789)", StrictMath.toRadians(dBig), 0x40a0d57474965437L);

        sectionEnd("strictd", 245);
    }

    // ========================================================================
    // 6. mathd — java.lang.Math's 40 double/float triples.
    //
    // The SAME surface as strictd, asked of the class with the LOOSER contract.
    // Math's transcendentals are specified to within 1 ulp (2 for pow and
    // atan2), so those rows use ckU and report the ulp distance rather than
    // demanding a bit pattern: failing a conforming implementation for
    // conforming would make this vector useless, and a 44-ulp defect is caught
    // just as well.
    //
    // Everything Math DOES specify exactly — abs, ceil, floor, rint, round,
    // signum, max, min, nextAfter/Up/Down, getExponent, IEEEremainder, and
    // every special value of every function — is asserted with ckD.
    //
    // Math.copySign is the one deliberate asymmetry: its javadoc explicitly
    // permits an implementation to treat a NaN sign argument as positive, so
    // the NaN-sign row asserts only the MAGNITUDE. StrictMath's twin, which
    // has no such licence, carries the exact assertion.
    // ========================================================================
    static void mathd() {
        // -- abs: MUST preserve the NaN class and MUST turn -0.0 into +0.0.
        ckD("mathd:abs(-0.0)", Math.abs(dNegZero), 0x0L);
        ckD("mathd:abs(0.0)", Math.abs(dZero), 0x0L);
        ckD("mathd:abs(-2.5)", Math.abs(dNeg2p5), 0x4004000000000000L);
        ckD("mathd:abs(-Inf)", Math.abs(dNegInf), 0x7ff0000000000000L);
        ckD("mathd:abs(NaN)", Math.abs(dNaN), 0x7ff8000000000000L);
        ckD("mathd:abs(-MIN_VALUE)", Math.abs(-dMin), 0x1L);
        ckF("mathd:abs(-0.0f)", Math.abs(fNegZero), 0x0);
        ckF("mathd:abs(-2.5f)", Math.abs(fNeg2p5), 0x40200000);
        ckF("mathd:abs(NaN f)", Math.abs(fNaN), 0x7fc00000);
        ckF("mathd:abs(-Inf f)", Math.abs(fNegInf), 0x7f800000);

        // -- the exactly-specified rounding family ----------------------------
        ckD("mathd:rint(2.5)", Math.rint(d2p5), 0x4000000000000000L);
        ckD("mathd:rint(3.5)", Math.rint(d3p5), 0x4010000000000000L);
        ckD("mathd:rint(-2.5)", Math.rint(dNeg2p5), 0xc000000000000000L);
        ckD("mathd:rint(-0.5)", Math.rint(dNegHalf), 0x8000000000000000L);
        ckD("mathd:rint(0.5)", Math.rint(dHalf), 0x0L);
        ckD("mathd:rint(NaN)", Math.rint(dNaN), 0x7ff8000000000000L);
        ckD("mathd:rint(-Inf)", Math.rint(dNegInf), 0xfff0000000000000L);
        ckJ("mathd:round(0.5)", Math.round(dHalf), 1L);
        ckJ("mathd:round(-0.5)", Math.round(dNegHalf), 0L);
        ckJ("mathd:round(2.5)", Math.round(d2p5), 3L);
        ckJ("mathd:round(-2.5)", Math.round(dNeg2p5), -2L);
        ckJ("mathd:round(0.49999999999999994)", Math.round(dJustBelowHalf), 0L);
        ckJ("mathd:round(NaN)", Math.round(dNaN), 0L);
        ckJ("mathd:round(+Inf)", Math.round(dInf), 9223372036854775807L);
        ckJ("mathd:round(-Inf)", Math.round(dNegInf), -9223372036854775808L);
        ckI("mathd:round(0.5f)", Math.round(fHalf), 1);
        ckI("mathd:round(-0.5f)", Math.round(fNegHalf), 0);
        ckI("mathd:round(0.49999997f)", Math.round(fJustBelowHalf), 0);
        ckI("mathd:round(NaN f)", Math.round(fNaN), 0);
        ckI("mathd:round(-Inf f)", Math.round(fNegInf), -2147483648);
        ckD("mathd:ceil(-0.5)", Math.ceil(dNegHalf), 0x8000000000000000L);
        ckD("mathd:ceil(-0.0)", Math.ceil(dNegZero), 0x8000000000000000L);
        ckD("mathd:ceil(2.5)", Math.ceil(d2p5), 0x4008000000000000L);
        ckD("mathd:ceil(NaN)", Math.ceil(dNaN), 0x7ff8000000000000L);
        ckD("mathd:floor(-0.0)", Math.floor(dNegZero), 0x8000000000000000L);
        ckD("mathd:floor(-2.5)", Math.floor(dNeg2p5), 0xc008000000000000L);
        ckD("mathd:floor(0.5)", Math.floor(dHalf), 0x0L);
        ckD("mathd:floor(+Inf)", Math.floor(dInf), 0x7ff0000000000000L);
        ckD("mathd:signum(-0.0)", Math.signum(dNegZero), 0x8000000000000000L);
        ckD("mathd:signum(0.0)", Math.signum(dZero), 0x0L);
        ckD("mathd:signum(NaN)", Math.signum(dNaN), 0x7ff8000000000000L);
        ckD("mathd:signum(-2.5)", Math.signum(dNeg2p5), 0xbff0000000000000L);
        ckF("mathd:signum(-0.0f)", Math.signum(fNegZero), 0x80000000);
        ckF("mathd:signum(NaN f)", Math.signum(fNaN), 0x7fc00000);
        ckF("mathd:signum(2.5f)", Math.signum(f2p5), 0x3f800000);

        // -- copySign. The NaN-sign row asserts MAGNITUDE ONLY; see the block comment.
        ckD("mathd:copySign(2.5,-0.0)", Math.copySign(d2p5, dNegZero), 0xc004000000000000L);
        ckD("mathd:copySign(-2.5,0.0)", Math.copySign(dNeg2p5, dZero), 0x4004000000000000L);
        ckD("mathd:copySign(-Inf,1.0)", Math.copySign(dNegInf, dOne), 0x7ff0000000000000L);
        ckD("mathd:copySign(NaN,-1.0)", Math.copySign(dNaN, dNegOne), 0xfff8000000000000L);
        ckD("mathd:abs of copySign(1.0,-NaN)", Math.abs(Math.copySign(dOne, dNegNaN)), 0x3ff0000000000000L);
        ckF("mathd:copySign(2.5f,-0.0f)", Math.copySign(f2p5, fNegZero), 0xc0200000);
        ckF("mathd:copySign(-1.0f,2.5f)", Math.copySign(fNegOne, f2p5), 0x3f800000);
        ckF("mathd:abs of copySign(1.0f,-NaN f)", Math.abs(Math.copySign(fOne, fNegNaN)), 0x3f800000);

        // -- max/min over signed zeros and NaN ---------------------------------
        ckD("mathd:max(-0.0,0.0)", Math.max(dNegZero, dZero), 0x0L);
        ckD("mathd:min(-0.0,0.0)", Math.min(dNegZero, dZero), 0x8000000000000000L);
        ckD("mathd:max(NaN,1.0)", Math.max(dNaN, dOne), 0x7ff8000000000000L);
        ckD("mathd:min(1.0,NaN)", Math.min(dOne, dNaN), 0x7ff8000000000000L);
        ckD("mathd:max(1.0,2.5)", Math.max(dOne, d2p5), 0x4004000000000000L);
        ckD("mathd:min(-Inf,MIN_VALUE)", Math.min(dNegInf, dMin), 0xfff0000000000000L);
        ckF("mathd:max(-0.0f,0.0f)", Math.max(fNegZero, fZero), 0x0);
        ckF("mathd:min(-0.0f,0.0f)", Math.min(fNegZero, fZero), 0x80000000);
        ckF("mathd:max(NaN f,1.0f)", Math.max(fNaN, fOne), 0x7fc00000);
        ckF("mathd:min(NaN f,1.0f)", Math.min(fNaN, fOne), 0x7fc00000);

        // -- the ulp-neighbourhood family, all exactly specified ----------------
        ckD("mathd:nextAfter(0.0,-1.0)", Math.nextAfter(dZero, dNegOne), 0x8000000000000001L);
        ckD("mathd:nextAfter(1.0,1.0)", Math.nextAfter(dOne, dOne), 0x3ff0000000000000L);
        ckD("mathd:nextAfter(MAX,+Inf)", Math.nextAfter(dMax, dInf), 0x7ff0000000000000L);
        ckD("mathd:nextAfter(NaN,1.0)", Math.nextAfter(dNaN, dOne), 0x7ff8000000000000L);
        ckD("mathd:nextUp(-0.0)", Math.nextUp(dNegZero), 0x1L);
        ckD("mathd:nextUp(-Inf)", Math.nextUp(dNegInf), 0xffefffffffffffffL);
        ckD("mathd:nextUp(1.0)", Math.nextUp(dOne), 0x3ff0000000000001L);
        ckD("mathd:nextDown(0.0)", Math.nextDown(dZero), 0x8000000000000001L);
        ckD("mathd:nextDown(+Inf)", Math.nextDown(dInf), 0x7fefffffffffffffL);
        ckD("mathd:nextDown(1.0)", Math.nextDown(dOne), 0x3fefffffffffffffL);
        ckI("mathd:getExponent(0.0)", Math.getExponent(dZero), -1023);
        ckI("mathd:getExponent(MIN_VALUE)", Math.getExponent(dMin), -1023);
        ckI("mathd:getExponent(NaN)", Math.getExponent(dNaN), 1024);
        ckI("mathd:getExponent(+Inf)", Math.getExponent(dInf), 1024);
        ckI("mathd:getExponent(MAX_VALUE)", Math.getExponent(dMax), 1023);

        // -- IEEEremainder is exact in Math too ---------------------------------
        ckD("mathd:IEEEremainder(5,3)", Math.IEEEremainder(dFive, dThree), 0xbff0000000000000L);
        ckD("mathd:IEEEremainder(-5,3)", Math.IEEEremainder(-dFive, dThree), 0x3ff0000000000000L);
        ckD("mathd:IEEEremainder(5,0)", Math.IEEEremainder(dFive, dZero), 0xfff8000000000000L);
        ckD("mathd:IEEEremainder(1,+Inf)", Math.IEEEremainder(dOne, dInf), 0x3ff0000000000000L);
        ckD("mathd:IEEEremainder(2.5,1.0)", Math.IEEEremainder(d2p5, dOne), 0x3fe0000000000000L);

        // -- from here the SPECIAL values stay exact, the INTERIOR gets a tolerance.
        ckD("mathd:sqrt(-0.0)", Math.sqrt(dNegZero), 0x8000000000000000L);
        ckD("mathd:sqrt(-1.0)", Math.sqrt(dNegOne), 0xfff8000000000000L);
        ckD("mathd:sqrt(+Inf)", Math.sqrt(dInf), 0x7ff0000000000000L);
        ckD("mathd:sqrt(NaN)", Math.sqrt(dNaN), 0x7ff8000000000000L);
        ckU("mathd:sqrt(2.0)", Math.sqrt(dTwo), 0x3ff6a09e667f3bcdL, 1);
        ckU("mathd:sqrt(0.1)", Math.sqrt(d0p1), 0x3fd43d136248490fL, 1);
        ckU("mathd:sqrt(MAX_VALUE)", Math.sqrt(dMax), 0x5fefffffffffffffL, 1);

        ckD("mathd:cbrt(-0.0)", Math.cbrt(dNegZero), 0x8000000000000000L);
        ckD("mathd:cbrt(-Inf)", Math.cbrt(dNegInf), 0xfff0000000000000L);
        ckD("mathd:cbrt(NaN)", Math.cbrt(dNaN), 0x7ff8000000000000L);
        ckU("mathd:cbrt(-8.0)", Math.cbrt(dNegEight), 0xc000000000000000L, 1);
        ckU("mathd:cbrt(0.1)", Math.cbrt(d0p1), 0x3fddb4c7760bcff3L, 1);
        ckU("mathd:cbrt(123456.789)", Math.cbrt(dBig), 0x4048e58dab7da808L, 1);

        ckD("mathd:hypot(+Inf,NaN)", Math.hypot(dInf, dNaN), 0x7ff0000000000000L);
        ckD("mathd:hypot(NaN,1.0)", Math.hypot(dNaN, dOne), 0x7ff8000000000000L);
        ckD("mathd:hypot(MAX,MAX)", Math.hypot(dMax, dMax), 0x7ff0000000000000L);
        ckU("mathd:hypot(3,4)", Math.hypot(dThree, dFour), 0x4014000000000000L, 1);
        ckU("mathd:hypot(0.1,0.7)", Math.hypot(d0p1, d0p7), 0x3fe6a09e667f3bccL, 1);

        ckD("mathd:pow(1.0,NaN)", Math.pow(dOne, dNaN), 0x7ff8000000000000L);
        ckD("mathd:pow(NaN,0.0)", Math.pow(dNaN, dZero), 0x3ff0000000000000L);
        ckD("mathd:pow(-1.0,+Inf)", Math.pow(dNegOne, dInf), 0x7ff8000000000000L);
        ckD("mathd:pow(0.0,-1.0)", Math.pow(dZero, dNegOne), 0x7ff0000000000000L);
        ckD("mathd:pow(-0.0,-3.0)", Math.pow(dNegZero, -dThree), 0xfff0000000000000L);
        ckD("mathd:pow(-2.0,3.5)", Math.pow(-dTwo, d3p5), 0xfff8000000000000L);
        // The interior. This is where the 44-ulp fast path lived while every special value
        // above was correct, so these rows are the point of the family.
        ckU("mathd:pow(2,10)", Math.pow(dTwo, dTen), 0x4090000000000000L, 2);
        ckU("mathd:pow(2,0.5)", Math.pow(dTwo, dHalf), 0x3ff6a09e667f3bcdL, 2);
        ckU("mathd:pow(0.1,3.0)", Math.pow(d0p1, dThree), 0x3f50624dd2f1a9fdL, 2);
        ckU("mathd:pow(3.0,0.7)", Math.pow(dThree, d0p7), 0x400142e81c889914L, 2);
        ckU("mathd:pow(1.0000000000000002,1e17)", Math.pow(dJustAbove1, d1e17), 0x41f06272889f21c5L, 2);
        ckU("mathd:pow(123456.789,0.25)", Math.pow(dBig, d0p25), 0x4032bea55de63d4fL, 2);
        ckU("mathd:pow(-2.0,3.0)", Math.pow(-dTwo, dThree), 0xc020000000000000L, 2);
        ckU("mathd:pow(1e300,2.0)", Math.pow(d1e300, dTwo), 0x7ff0000000000000L, 2);

        ckD("mathd:exp(-Inf)", Math.exp(dNegInf), 0x0L);
        ckD("mathd:exp(+Inf)", Math.exp(dInf), 0x7ff0000000000000L);
        ckD("mathd:exp(NaN)", Math.exp(dNaN), 0x7ff8000000000000L);
        ckD("mathd:exp(1000.0)", Math.exp(dThousand), 0x7ff0000000000000L);
        ckU("mathd:exp(0.0)", Math.exp(dZero), 0x3ff0000000000000L, 1);
        ckU("mathd:exp(1.0)", Math.exp(dOne), 0x4005bf0a8b145769L, 1);
        ckU("mathd:exp(0.1)", Math.exp(d0p1), 0x3ff1aec7b35a00d4L, 1);
        ckU("mathd:exp(-2.5)", Math.exp(dNeg2p5), 0x3fb50385c094f425L, 1);
        ckD("mathd:expm1(-0.0)", Math.expm1(dNegZero), 0x8000000000000000L);
        ckD("mathd:expm1(-Inf)", Math.expm1(dNegInf), 0xbff0000000000000L);
        ckU("mathd:expm1(1e-300)", Math.expm1(d1eNeg300), 0x1a56e1fc2f8f359L, 1);
        ckU("mathd:expm1(0.1)", Math.expm1(d0p1), 0x3fbaec7b35a00d3aL, 1);
        ckU("mathd:expm1(1.0)", Math.expm1(dOne), 0x3ffb7e151628aed2L, 1);

        ckD("mathd:log(0.0)", Math.log(dZero), 0xfff0000000000000L);
        ckD("mathd:log(-0.0)", Math.log(dNegZero), 0xfff0000000000000L);
        ckD("mathd:log(-1.0)", Math.log(dNegOne), 0xfff8000000000000L);
        ckD("mathd:log(+Inf)", Math.log(dInf), 0x7ff0000000000000L);
        ckU("mathd:log(1.0)", Math.log(dOne), 0x0L, 1);
        ckU("mathd:log(E)", Math.log(dE), 0x3ff0000000000000L, 1);
        ckU("mathd:log(0.1)", Math.log(d0p1), 0xc0026bb1bbb55515L, 1);
        ckU("mathd:log(123456.789)", Math.log(dBig), 0x40277281cad8a844L, 1);
        ckD("mathd:log10(0.0)", Math.log10(dZero), 0xfff0000000000000L);
        ckD("mathd:log10(-1.0)", Math.log10(dNegOne), 0xfff8000000000000L);
        ckU("mathd:log10(1000.0)", Math.log10(dThousand), 0x4008000000000000L, 1);
        ckU("mathd:log10(0.1)", Math.log10(d0p1), 0xbff0000000000000L, 1);
        ckU("mathd:log10(123456.789)", Math.log10(dBig), 0x40145db61a282512L, 1);
        ckD("mathd:log1p(-1.0)", Math.log1p(dNegOne), 0xfff0000000000000L);
        ckD("mathd:log1p(-2.0)", Math.log1p(-dTwo), 0x7ff8000000000000L);
        ckD("mathd:log1p(-0.0)", Math.log1p(dNegZero), 0x8000000000000000L);
        ckU("mathd:log1p(1e-300)", Math.log1p(d1eNeg300), 0x1a56e1fc2f8f359L, 1);
        ckU("mathd:log1p(0.1)", Math.log1p(d0p1), 0x3fb8663f793c46c7L, 1);

        ckD("mathd:sin(-0.0)", Math.sin(dNegZero), 0x8000000000000000L);
        ckD("mathd:sin(+Inf)", Math.sin(dInf), 0xfff8000000000000L);
        ckD("mathd:sin(NaN)", Math.sin(dNaN), 0x7ff8000000000000L);
        ckU("mathd:sin(1.0)", Math.sin(dOne), 0x3feaed548f090ceeL, 1);
        ckU("mathd:sin(PI)", Math.sin(dPi), 0x3ca1a62633145c07L, 1);
        ckU("mathd:sin(0.1)", Math.sin(d0p1), 0x3fb98eaecb8bcb2cL, 1);
        ckU("mathd:sin(123456.789)", Math.sin(dBig), 0xbfeff50e60ab53f9L, 1);
        ckU("mathd:sin(1e17)", Math.sin(d1e17), 0xbfddbadc7a119fc8L, 1);
        ckD("mathd:cos(-Inf)", Math.cos(dNegInf), 0xfff8000000000000L);
        ckD("mathd:cos(NaN)", Math.cos(dNaN), 0x7ff8000000000000L);
        ckU("mathd:cos(0.0)", Math.cos(dZero), 0x3ff0000000000000L, 1);
        ckU("mathd:cos(1.0)", Math.cos(dOne), 0x3fe14a280fb5068cL, 1);
        ckU("mathd:cos(PI)", Math.cos(dPi), 0xbff0000000000000L, 1);
        ckU("mathd:cos(123456.789)", Math.cos(dBig), 0x3faa74d27c41b22aL, 1);
        ckD("mathd:tan(-0.0)", Math.tan(dNegZero), 0x8000000000000000L);
        ckD("mathd:tan(+Inf)", Math.tan(dInf), 0xfff8000000000000L);
        ckU("mathd:tan(1.0)", Math.tan(dOne), 0x3ff8eb245cbee3a6L, 1);
        ckU("mathd:tan(PI)", Math.tan(dPi), 0xbca1a62633145c07L, 1);
        ckU("mathd:tan(123456.789)", Math.tan(dBig), 0xc03353a85fe8d6d5L, 1);
        ckD("mathd:asin(-0.0)", Math.asin(dNegZero), 0x8000000000000000L);
        ckD("mathd:asin(2.0)", Math.asin(dTwo), 0xfff8000000000000L);
        ckU("mathd:asin(1.0)", Math.asin(dOne), 0x3ff921fb54442d18L, 1);
        ckU("mathd:asin(0.7)", Math.asin(d0p7), 0x3fe8d00e692afd95L, 1);
        ckD("mathd:acos(2.0)", Math.acos(dTwo), 0xfff8000000000000L);
        ckU("mathd:acos(1.0)", Math.acos(dOne), 0x0L, 1);
        ckU("mathd:acos(-1.0)", Math.acos(dNegOne), 0x400921fb54442d18L, 1);
        ckU("mathd:acos(0.7)", Math.acos(d0p7), 0x3fe973e83f5d5c9bL, 1);
        ckD("mathd:atan(-0.0)", Math.atan(dNegZero), 0x8000000000000000L);
        ckD("mathd:atan(+Inf)", Math.atan(dInf), 0x3ff921fb54442d18L);
        ckU("mathd:atan(1.0)", Math.atan(dOne), 0x3fe921fb54442d18L, 1);
        ckU("mathd:atan(0.1)", Math.atan(d0p1), 0x3fb983e282e2cc4dL, 1);
        ckD("mathd:atan2(0.0,-0.0)", Math.atan2(dZero, dNegZero), 0x400921fb54442d18L);
        ckD("mathd:atan2(-0.0,-0.0)", Math.atan2(dNegZero, dNegZero), 0xc00921fb54442d18L);
        ckD("mathd:atan2(-0.0,1.0)", Math.atan2(dNegZero, dOne), 0x8000000000000000L);
        ckD("mathd:atan2(1.0,NaN)", Math.atan2(dOne, dNaN), 0x7ff8000000000000L);
        ckU("mathd:atan2(+Inf,+Inf)", Math.atan2(dInf, dInf), 0x3fe921fb54442d18L, 2);
        ckU("mathd:atan2(1.0,2.0)", Math.atan2(dOne, dTwo), 0x3fddac670561bb4fL, 2);
        ckU("mathd:atan2(-3.5,0.7)", Math.atan2(dNeg3p5, d0p7), 0xbff5f97315254857L, 2);

        ckD("mathd:sinh(-0.0)", Math.sinh(dNegZero), 0x8000000000000000L);
        ckD("mathd:sinh(+Inf)", Math.sinh(dInf), 0x7ff0000000000000L);
        ckD("mathd:sinh(1000.0)", Math.sinh(dThousand), 0x7ff0000000000000L);
        ckU("mathd:sinh(1.0)", Math.sinh(dOne), 0x3ff2cd9fc44eb982L, 1);
        ckU("mathd:sinh(0.1)", Math.sinh(d0p1), 0x3fb9a487337b59b3L, 1);
        ckD("mathd:cosh(-Inf)", Math.cosh(dNegInf), 0x7ff0000000000000L);
        ckD("mathd:cosh(1000.0)", Math.cosh(dThousand), 0x7ff0000000000000L);
        ckU("mathd:cosh(0.0)", Math.cosh(dZero), 0x3ff0000000000000L, 1);
        ckU("mathd:cosh(1.0)", Math.cosh(dOne), 0x3ff8b07551d9f551L, 1);
        ckU("mathd:cosh(0.7)", Math.cosh(d0p7), 0x3ff4152c1862342fL, 1);
        ckD("mathd:tanh(-0.0)", Math.tanh(dNegZero), 0x8000000000000000L);
        ckD("mathd:tanh(+Inf)", Math.tanh(dInf), 0x3ff0000000000000L);
        ckD("mathd:tanh(-Inf)", Math.tanh(dNegInf), 0xbff0000000000000L);
        ckU("mathd:tanh(1.0)", Math.tanh(dOne), 0x3fe85efab514f394L, 1);
        ckU("mathd:tanh(0.1)", Math.tanh(d0p1), 0x3fb983d7795f413aL, 1);

        ckD("mathd:toDegrees(-0.0)", Math.toDegrees(dNegZero), 0x8000000000000000L);
        ckD("mathd:toDegrees(NaN)", Math.toDegrees(dNaN), 0x7ff8000000000000L);
        ckD("mathd:toDegrees(+Inf)", Math.toDegrees(dInf), 0x7ff0000000000000L);
        ckU("mathd:toDegrees(PI)", Math.toDegrees(dPi), 0x4066800000000000L, 2);
        ckU("mathd:toDegrees(1.0)", Math.toDegrees(dOne), 0x404ca5dc1a63c1f8L, 2);
        ckD("mathd:toRadians(-0.0)", Math.toRadians(dNegZero), 0x8000000000000000L);
        ckD("mathd:toRadians(NaN)", Math.toRadians(dNaN), 0x7ff8000000000000L);
        ckU("mathd:toRadians(180.0)", Math.toRadians(dHundred + 80.0), 0x400921fb54442d18L, 2);
        ckU("mathd:toRadians(123456.789)", Math.toRadians(dBig), 0x40a0d57474965437L, 2);

        // The two classes must agree wherever BOTH are exactly specified. This needs no stored
        // expectation at all, so it survives every edit to the numbers above.
        check(Double.doubleToRawLongBits(Math.rint(d2p5))
                        == Double.doubleToRawLongBits(StrictMath.rint(d2p5)),
                "mathd: Math.rint and StrictMath.rint must agree at 2.5");
        check(Double.doubleToRawLongBits(Math.IEEEremainder(dFive, dThree))
                        == Double.doubleToRawLongBits(StrictMath.IEEEremainder(dFive, dThree)),
                "mathd: Math.IEEEremainder and StrictMath.IEEEremainder must agree at (5,3)");
        check(Math.round(dJustBelowHalf) == StrictMath.round(dJustBelowHalf),
                "mathd: the two round(double) must agree at 0.49999999999999994");

        sectionEnd("mathd", 211);
    }

    // ========================================================================
    // 7. bigdec — java.math.BigDecimal, 19 triples.
    //
    // W8-C3-1 left this family for "its own lane" on the grounds that rounding
    // modes x scale is a matrix. That is true of an EXHAUSTIVE treatment; it is
    // not a reason for a registered native to have no committed caller at all.
    // The rows below take the edges where a Rust body cannot help but differ:
    //
    //   * new BigDecimal(double) is the EXACT binary expansion of the double
    //     (0.1 becomes a 55-digit decimal); BigDecimal.valueOf(double) is
    //     Double.toString's shortest round trip. One class, two conversions,
    //     and a body that implements one of them for both is wrong for half
    //     its callers.
    //   * setScale(int) THROWS ArithmeticException when rounding would be
    //     needed. Hazard 1: a Rust body that reaches for a checked division
    //     panics where Java unwinds.
    //   * intValue()/longValue() are NARROWING (mod 2^32 / 2^64), NOT
    //     saturating -- the opposite of Double.intValue in boxconv().
    //   * toString() uses scientific notation at scale boundaries and
    //     toPlainString() never does.
    // ========================================================================
    static void bigdec() {
        // -- the two double conversions, which must NOT agree -----------------
        BigDecimal exact = new BigDecimal(d0p1);
        BigDecimal shortest = BigDecimal.valueOf(d0p1);
        ckS("bigdec:new BigDecimal(0.1).toString", exact.toString(), "0.1000000000000000055511151231257827021181583404541015625");
        ckS("bigdec:BigDecimal.valueOf(0.1).toString", shortest.toString(), "0.1");
        check(!exact.toString().equals(shortest.toString()),
                "bigdec: new BigDecimal(double) is the EXACT binary expansion and"
                        + " BigDecimal.valueOf(double) is the shortest round trip; a VM where"
                        + " they print the same string has one body serving both");
        ckI("bigdec:new BigDecimal(0.1).scale", exact.scale(), 55);
        ckI("bigdec:new BigDecimal(0.1).precision", exact.precision(), 55);
        ckI("bigdec:BigDecimal.valueOf(0.1).scale", shortest.scale(), 1);
        ckS("bigdec:new BigDecimal(0.5).toString", new BigDecimal(dHalf).toString(), "0.5");
        ckS("bigdec:BigDecimal.valueOf(1e7).toString", BigDecimal.valueOf(d1e7).toString(), "1.0E+7");
        ckS("bigdec:BigDecimal.valueOf(-0.0).toString", BigDecimal.valueOf(dNegZero).toString(),
                "0.0");
        ckS("bigdec:BigDecimal.valueOf(100L).toString", BigDecimal.valueOf(100L).toString(), "100");
        ckI("bigdec:BigDecimal.valueOf(100L).scale", BigDecimal.valueOf(100L).scale(), 0);
        ckS("bigdec:BigDecimal.valueOf(MIN_LONG).toString", BigDecimal.valueOf(jMin).toString(),
                "-9223372036854775808");
        ckS("bigdec:new BigDecimal(BigInteger.TEN).toString",
                new BigDecimal(BigInteger.TEN).toString(), "10");

        // new BigDecimal(NaN) / (Infinity) must THROW, not produce a value.
        Throwable t = null;
        try {
            sinkO = new BigDecimal(dNaN);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:new BigDecimal(NaN)", t, "java.lang.NumberFormatException");
        t = null;
        try {
            sinkO = new BigDecimal(dInf);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:new BigDecimal(+Inf)", t, "java.lang.NumberFormatException");
        t = null;
        try {
            sinkO = BigDecimal.valueOf(dNaN);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:BigDecimal.valueOf(NaN)", t, "java.lang.NumberFormatException");
        // Both infinities, because valueOf's guard is one `isFinite` test and a
        // fix that checks only NaN passes the row above while leaving these two
        // answering a NUMBER. Measured: all three returned 0.
        t = null;
        try {
            sinkO = BigDecimal.valueOf(dInf);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:BigDecimal.valueOf(+Inf)", t, "java.lang.NumberFormatException");
        ckS("bigdec:BigDecimal.valueOf(+Inf) message", t == null ? null : t.getMessage(),
                "Infinite or NaN");
        t = null;
        try {
            sinkO = BigDecimal.valueOf(dNegInf);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:BigDecimal.valueOf(-Inf)", t, "java.lang.NumberFormatException");

        // -- toString vs toPlainString ----------------------------------------
        BigDecimal sci = new BigDecimal("1E+10");
        ckS("bigdec:new BigDecimal(\"1E+10\").toString", sci.toString(), "1E+10");
        ckS("bigdec:new BigDecimal(\"1E+10\").toPlainString", sci.toPlainString(), "10000000000");
        ckI("bigdec:new BigDecimal(\"1E+10\").scale", sci.scale(), -10);
        BigDecimal tiny = new BigDecimal("1E-10");
        ckS("bigdec:new BigDecimal(\"1E-10\").toString", tiny.toString(), "1E-10");
        ckS("bigdec:new BigDecimal(\"1E-10\").toPlainString", tiny.toPlainString(), "0.0000000001");
        ckS("bigdec:new BigDecimal(\"0.0001\").toString", new BigDecimal("0.0001").toString(), "0.0001");
        ckS("bigdec:new BigDecimal(\"0.00001\").toString", new BigDecimal("0.00001").toString(),
                "0.00001");

        // -- the arithmetic, where the SCALE is as specified as the value ------
        BigDecimal a = new BigDecimal("1.5");
        BigDecimal b = new BigDecimal("1.50");
        ckS("bigdec:1.5 add 1.50", a.add(b).toString(), "3.00");
        ckI("bigdec:1.5 add 1.50 scale", a.add(b).scale(), 2);
        ckS("bigdec:1.5 subtract 1.50", a.subtract(b).toString(), "0.00");
        ckS("bigdec:1.5 multiply 1.50", a.multiply(b).toString(), "2.250");
        ckI("bigdec:1.5 multiply 1.50 scale", a.multiply(b).scale(), 3);
        ckS("bigdec:1.5 negate", a.negate().toString(), "-1.5");
        ckS("bigdec:0.00 negate", new BigDecimal("0.00").negate().toString(), "0.00");
        ckI("bigdec:-0.00 signum", new BigDecimal("-0.00").signum(), 0);
        ckI("bigdec:1.5 signum", a.signum(), 1);
        ckI("bigdec:-1.5 signum", a.negate().signum(), -1);
        ckI("bigdec:0.00 precision", new BigDecimal("0.00").precision(), 1);
        ckI("bigdec:1.50 precision", b.precision(), 3);

        // -- setScale: the ArithmeticException path (hazard 1) -----------------
        ckS("bigdec:setScale(3) widening", a.setScale(3).toString(), "1.500");
        t = null;
        try {
            sinkO = new BigDecimal("2.5").setScale(0);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:setScale(0) on 2.5 without a mode", t, "java.lang.ArithmeticException");
        // The int-mode overload: 4 is ROUND_HALF_UP, 6 is ROUND_HALF_EVEN. They must DIFFER
        // at 2.5, which is what makes this a test of the mode rather than of the rounding.
        ckS("bigdec:2.5 setScale(0,HALF_UP=4)", new BigDecimal("2.5").setScale(0, 4).toString(),
                "3");
        ckS("bigdec:2.5 setScale(0,HALF_EVEN=6)", new BigDecimal("2.5").setScale(0, 6).toString(),
                "2");
        ckS("bigdec:3.5 setScale(0,HALF_EVEN=6)", new BigDecimal("3.5").setScale(0, 6).toString(),
                "4");
        ckS("bigdec:-2.5 setScale(0,HALF_UP=4)", new BigDecimal("-2.5").setScale(0, 4).toString(),
                "-3");
        ckS("bigdec:2.5 setScale(0,FLOOR=3)", new BigDecimal("2.5").setScale(0, 3).toString(), "2");
        ckS("bigdec:-2.5 setScale(0,FLOOR=3)", new BigDecimal("-2.5").setScale(0, 3).toString(),
                "-3");
        t = null;
        try {
            sinkO = new BigDecimal("2.5").setScale(0, 99);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigdec:setScale(0, bad mode 99)", t, "java.lang.IllegalArgumentException");

        // -- the NARROWING conversions -----------------------------------------
        BigDecimal huge = new BigDecimal("100000000000000000000");
        ckI("bigdec:1e20 intValue (narrowing, NOT saturating)", huge.intValue(), 1661992960);
        ckJ("bigdec:1e20 longValue (narrowing)", huge.longValue(), 7766279631452241920L);
        ckI("bigdec:2.9 intValue", new BigDecimal("2.9").intValue(), 2);
        ckI("bigdec:-2.9 intValue", new BigDecimal("-2.9").intValue(), -2);
        ckJ("bigdec:-2.9 longValue", new BigDecimal("-2.9").longValue(), -2L);
        ckD("bigdec:1e400 doubleValue", new BigDecimal("1E+400").doubleValue(), 0x7ff0000000000000L);
        ckD("bigdec:-1e400 doubleValue", new BigDecimal("-1E+400").doubleValue(), 0xfff0000000000000L);
        ckD("bigdec:1e-400 doubleValue", new BigDecimal("1E-400").doubleValue(), 0x0L);
        ckD("bigdec:0.1 doubleValue round trip", shortest.doubleValue(), 0x3fb999999999999aL);
        ckS("bigdec:2.9 toBigInteger", new BigDecimal("2.9").toBigInteger().toString(), "2");
        ckS("bigdec:-2.9 toBigInteger", new BigDecimal("-2.9").toBigInteger().toString(), "-2");
        ckS("bigdec:1E+10 toBigInteger", sci.toBigInteger().toString(), "10000000000");

        sectionEnd("bigdec", 59);
    }

    // ========================================================================
    // 8. bigint — java.math.BigInteger, 18 triples.
    //
    // Thirteen of the eighteen are ordinary API. The other five —
    // implMulAdd, implSquareToLen, mulAdd, shiftLeftImplWorker,
    // shiftRightImplWorker — are package-private array kernels that ordinary
    // Java source CANNOT NAME. They are reachable only INDIRECTLY, through
    // arithmetic large enough for BigInteger to select them, and a check
    // written that way cannot prove which body answered: it measures its own
    // reach. ([reach!=defect].)
    //
    // So the kernel rows below are deliberately shaped as ROUND TRIPS and
    // INVARIANTS rather than as stored digit strings — x.shiftLeft(n) then
    // shiftRight(n) must be x, and (a*b)/b must be a. An invariant that holds
    // is weak evidence the kernel is right; an invariant that BREAKS is strong
    // evidence it is wrong, which is the useful direction. The honest
    // instrument for these five is a registry dump before and after, comparing
    // the `invocations` column — see W8-C3-1's N4.
    // ========================================================================
    static void bigint() {
        ckS("bigint:valueOf(MIN_LONG).toString", BigInteger.valueOf(jMin).toString(), "-9223372036854775808");
        ckS("bigint:valueOf(MIN_LONG).negate", BigInteger.valueOf(jMin).negate().toString(), "9223372036854775808");
        ckS("bigint:valueOf(0).toString", BigInteger.valueOf(jZero).toString(), "0");
        ckS("bigint:valueOf(-1).toString", BigInteger.valueOf(jNegOne).toString(), "-1");
        ckI("bigint:ZERO.signum", BigInteger.ZERO.signum(), 0);
        ckI("bigint:valueOf(MIN_LONG).signum", BigInteger.valueOf(jMin).signum(), -1);
        ckI("bigint:ZERO.negate().signum", BigInteger.ZERO.negate().signum(), 0);

        // Narrowing, not saturating -- the same rule as BigDecimal and the OPPOSITE of Double.
        BigInteger big = BigInteger.ONE.shiftLeft(100);
        ckI("bigint:2^100 intValue", big.intValue(), 0);
        ckJ("bigint:2^100 longValue", big.longValue(), 0L);
        ckI("bigint:2^40 intValue", BigInteger.ONE.shiftLeft(40).intValue(), 0);
        ckI("bigint:valueOf(MIN_LONG).intValue", BigInteger.valueOf(jMin).intValue(), 0);
        ckJ("bigint:(2^100 + 7).longValue",
                big.add(BigInteger.valueOf(7L)).longValue(), 7L);

        // equals is TYPE-SENSITIVE, exactly like the boxes.
        ckB("bigint:ONE.equals(Long.valueOf(1))", BigInteger.ONE.equals(Long.valueOf(jOne)), false);
        ckB("bigint:ONE.equals(BigInteger.valueOf(1))",
                BigInteger.ONE.equals(BigInteger.valueOf(jOne)), true);
        ckB("bigint:ONE.equals(new BigDecimal(1))", BigInteger.ONE.equals(new BigDecimal(1)), false);
        ckI("bigint:valueOf(MIN).compareTo(valueOf(MAX))",
                BigInteger.valueOf(jMin).compareTo(BigInteger.valueOf(jMax)), -1);
        ckI("bigint:ZERO.compareTo(ZERO)", BigInteger.ZERO.compareTo(BigInteger.ZERO), 0);
        // The Object-typed overload is a separate registered triple from the BigInteger one.
        java.util.List<BigInteger> sorted = new ArrayList<>(Arrays.asList(
                BigInteger.valueOf(3L), BigInteger.valueOf(-7L), BigInteger.ZERO));
        java.util.Collections.sort(sorted);
        ckS("bigint:sorted via compareTo", sorted.toString(), "[-7, 0, 3]");

        // -- add/subtract across the sign boundary ------------------------------
        ckS("bigint:MIN_LONG + MIN_LONG",
                BigInteger.valueOf(jMin).add(BigInteger.valueOf(jMin)).toString(), "-18446744073709551616");
        ckS("bigint:MAX_LONG - MIN_LONG",
                BigInteger.valueOf(jMax).subtract(BigInteger.valueOf(jMin)).toString(), "18446744073709551615");
        ckS("bigint:(-7) + 7", BigInteger.valueOf(-7L).add(BigInteger.valueOf(7L)).toString(), "0");
        ckS("bigint:MAX_LONG * MAX_LONG",
                BigInteger.valueOf(jMax).multiply(BigInteger.valueOf(jMax)).toString(), "85070591730234615847396907784232501249");
        ckS("bigint:2^10 pow 0", BigInteger.valueOf(1024L).pow(0).toString(), "1");
        ckS("bigint:3 pow 5", BigInteger.valueOf(3L).pow(5).toString(), "243");
        Throwable t = null;
        try {
            sinkO = BigInteger.TWO.pow(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bigint:TWO.pow(-1)", t, "java.lang.ArithmeticException");

        // -- the INDIRECT kernel rows ------------------------------------------
        // Operands large enough that BigInteger selects squareToLen/multiplyToLen and the
        // shift workers rather than a small-value fast path. These assert INVARIANTS, not
        // stored digits, because the check cannot prove which body answered.
        BigInteger p = BigInteger.valueOf(3L).pow(2000);
        BigInteger q = BigInteger.valueOf(7L).pow(1500);
        ckI("bigint:3^2000 bitLength", p.bitLength(), 3170);
        ckI("bigint:3^2000 toString length", p.toString().length(), 955);
        ckS("bigint:3^2000 first 20 digits", p.toString().substring(0, 20), "17478712517226516096");
        ckI("bigint:(3^2000 * 7^1500) bitLength", p.multiply(q).bitLength(), 7381);
        ckS("bigint:(3^2000 * 7^1500) last 20 digits",
                p.multiply(q).toString().substring(p.multiply(q).toString().length() - 20), "34807550982031340001");
        check(p.multiply(q).divide(q).equals(p),
                "bigint: (3^2000 * 7^1500) / 7^1500 must be 3^2000 -- the multiply kernel and"
                        + " the divide kernel must agree, which no stored digit string could"
                        + " check without also trusting one of them");
        check(p.multiply(p).equals(p.pow(2)),
                "bigint: x*x and x.pow(2) must agree at 3^2000 -- multiplyToLen and squareToLen"
                        + " are DIFFERENT kernels reached by these two expressions");
        check(p.shiftLeft(1000).shiftRight(1000).equals(p),
                "bigint: shiftLeft(1000) then shiftRight(1000) must be the identity on 3^2000 --"
                        + " the two shift workers are separate registered triples and this is"
                        + " the only expression that drives both");
        check(p.shiftLeft(64).equals(p.multiply(BigInteger.ONE.shiftLeft(64))),
                "bigint: shiftLeft(64) must equal multiplying by 2^64 -- a WORD-ALIGNED shift,"
                        + " which is the worker's special case");
        check(p.shiftLeft(65).equals(p.multiply(BigInteger.TWO.pow(65))),
                "bigint: shiftLeft(65) must equal multiplying by 2^65 -- deliberately NOT"
                        + " word-aligned, so it takes the worker's general path");
        // shiftRight on a NEGATIVE value rounds toward NEGATIVE INFINITY, not toward zero --
        // an arithmetic shift, not a magnitude shift. 3^2000 is odd, so for n = -3^2000 the
        // floor is exactly (n - 1) / 2, and BigInteger.divide (which truncates) computes it
        // without rounding because n - 1 is even. A body that shifts the magnitude and
        // reapplies the sign is off by one here and nowhere else.
        BigInteger neg = p.negate();
        check(neg.testBit(0), "bigint: the shiftRight witness needs an ODD operand; 3^2000 is odd");
        check(neg.shiftRight(1).equals(neg.subtract(BigInteger.ONE).divide(BigInteger.TWO)),
                "bigint: (-3^2000) >> 1 must round toward NEGATIVE INFINITY");
        ckI("bigint:(-3^2000) shiftRight 1 signum", neg.shiftRight(1).signum(), -1);

        sectionEnd("bigint", 38);
    }

    // ========================================================================
    // 9. logrec — java.util.logging.LogRecord (19) + Handler (4), 23 triples.
    //
    // W8-C3-1 left LogRecord out because "it is a mutable bean; the risk is
    // state, and this file tests values". That is the reason to test it, not
    // the reason to skip it: a native-backed getter whose setter writes
    // somewhere else is invisible to a value-only census, and the recorded
    // pattern [nat hidden] is exactly that shape. Every getter below is
    // therefore asked TWICE — once for its constructed default, once after its
    // setter — because a getter that answers a constant passes the first ask.
    //
    // The three fields that are not caller-supplied (millis, sequence number,
    // thread id) are asserted RELATIONALLY: a stored value would be a clock
    // reading, and a fixture that pins a clock reading fails tomorrow.
    // ========================================================================
    static void logrec() {
        LogRecord r = new LogRecord(Level.INFO, "hello");

        // -- constructed defaults ---------------------------------------------
        ckS("logrec:getLevel().getName()", r.getLevel().getName(), "INFO");
        ckS("logrec:getMessage()", r.getMessage(), "hello");
        ckS("logrec:getLoggerName() default", r.getLoggerName(), null);
        ckB("logrec:getParameters() default is null", r.getParameters() == null, true);
        ckB("logrec:getResourceBundle() default is null", r.getResourceBundle() == null, true);
        ckS("logrec:getResourceBundleName() default", r.getResourceBundleName(), null);
        ckB("logrec:getThrown() default is null", r.getThrown() == null, true);

        // -- the clock/counter fields, asserted as RELATIONS -------------------
        long before = System.currentTimeMillis();
        LogRecord r2 = new LogRecord(Level.WARNING, "second");
        long after = System.currentTimeMillis();
        check(r2.getMillis() >= before - 1000 && r2.getMillis() <= after + 1000,
                "logrec: getMillis() must be a reading of the wall clock taken at construction,"
                        + " got " + r2.getMillis() + " outside [" + before + ", " + after + "]");
        check(r2.getSequenceNumber() > r.getSequenceNumber(),
                "logrec: sequence numbers must STRICTLY INCREASE across constructions, got "
                        + r.getSequenceNumber() + " then " + r2.getSequenceNumber());
        // A BOUND rather than an equality: the counter is process-global, so anything else that
        // logs between these two constructions widens the gap legitimately. The bound still
        // fails a counter that jumps, and the strict-increase check above still fails one that
        // does not move at all.
        check(r2.getSequenceNumber() - r.getSequenceNumber() <= 64L,
                "logrec: two LogRecords constructed back to back must be CLOSE in sequence"
                        + " number, got a gap of " + (r2.getSequenceNumber()
                        - r.getSequenceNumber()));
        check(r.getThreadID() == r2.getThreadID(),
                "logrec: two records made on the SAME thread must carry the same thread id, got "
                        + r.getThreadID() + " and " + r2.getThreadID());

        // -- every setter, then its getter -------------------------------------
        r.setLoggerName("com.example.Logger");
        ckS("logrec:getLoggerName() after set", r.getLoggerName(), "com.example.Logger");
        r.setLoggerName(null);
        ckS("logrec:getLoggerName() after set(null)", r.getLoggerName(), null);

        r.setMessage("replaced");
        ckS("logrec:getMessage() after set", r.getMessage(), "replaced");
        r.setMessage(null);
        ckS("logrec:getMessage() after set(null)", r.getMessage(), null);

        r.setMillis(1610712000000L);
        ckJ("logrec:getMillis() after set", r.getMillis(), 1610712000000L);

        r.setSequenceNumber(4242L);
        ckJ("logrec:getSequenceNumber() after set", r.getSequenceNumber(), 4242L);

        r.setThreadID(77);
        ckI("logrec:getThreadID() after set", r.getThreadID(), 77);

        Object[] params = {"a", Integer.valueOf(1), null};
        r.setParameters(params);
        ckB("logrec:getParameters() is non-null after set", r.getParameters() != null, true);
        ckI("logrec:getParameters().length", r.getParameters().length, 3);
        ckS("logrec:getParameters()[0]", (String) r.getParameters()[0], "a");
        ckB("logrec:getParameters()[2] is null", r.getParameters()[2] == null, true);
        r.setParameters(null);
        ckB("logrec:getParameters() null after set(null)", r.getParameters() == null, true);

        Throwable boom = new IllegalStateException("boom");
        r.setThrown(boom);
        check(r.getThrown() == boom,
                "logrec: getThrown() must return the SAME throwable that was set, by identity");
        ckS("logrec:getThrown().getMessage()", r.getThrown().getMessage(), "boom");
        r.setThrown(null);
        ckB("logrec:getThrown() null after set(null)", r.getThrown() == null, true);

        // setResourceBundleName and setResourceBundle are separate fields, and setting the
        // NAME must not fabricate a bundle.
        r.setResourceBundleName("some.bundle.Name");
        ckS("logrec:getResourceBundleName() after set", r.getResourceBundleName(), "some.bundle.Name");
        ckB("logrec:getResourceBundle() still null after setting only the NAME",
                r.getResourceBundle() == null, true);
        r.setResourceBundle(null);
        ckB("logrec:getResourceBundle() null after set(null)", r.getResourceBundle() == null, true);
        ckS("logrec:getResourceBundleName() survives setResourceBundle(null)",
                r.getResourceBundleName(), "some.bundle.Name");

        // -- Handler: the four registered lifecycle triples ---------------------
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        Handler h = new StreamHandler(out, new SimpleFormatter());
        h.setLevel(Level.WARNING);
        ckS("logrec:Handler.getLevel() after setLevel(WARNING)", h.getLevel().getName(), "WARNING");
        h.setLevel(Level.ALL);
        ckS("logrec:Handler.getLevel() after setLevel(ALL)", h.getLevel().getName(), "ALL");
        Throwable t = null;
        try {
            h.setLevel(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("logrec:Handler.setLevel(null)", t, "java.lang.NullPointerException");
        ckS("logrec:Handler.getLevel() unchanged by the failed set", h.getLevel().getName(), "ALL");
        t = null;
        try {
            h.flush();
            h.close();
            h.close();
        } catch (Throwable x) {
            t = x;
        }
        ckX("logrec:Handler flush + close + close again", t, "none");

        sectionEnd("logrec", 35);
    }

    /**
     * A {@link java.util.function.Supplier} as a NAMED class rather than a lambda. Nothing in
     * this file uses {@code invokedynamic}: a vector for the intrinsic census must not be able
     * to fail because {@code LambdaMetafactory} is weak on the VM under test, because that
     * failure would be indistinguishable from the intrinsic defect it is hunting.
     */
    static final class Sup implements java.util.function.Supplier<String> {
        private final String v;

        Sup(String v) {
            this.v = v;
        }

        @Override
        public String get() {
            return v;
        }
    }

    /** Counts how many times the JDK asked for an initial value. */
    static class CountingTL extends ThreadLocal<String> {
        int inits;

        @Override
        protected String initialValue() {
            inits++;
            return "init";
        }

        /** Reaches ThreadLocal.initialValue ITSELF, which the override above hides. */
        String superInitialValue() {
            return super.initialValue();
        }
    }

    static class CountingITL extends InheritableThreadLocal<String> {
        int inits;

        @Override
        protected String initialValue() {
            inits++;
            return "parent-init";
        }

        String superInitialValue() {
            return super.initialValue();
        }
    }

    // ========================================================================
    // 10. tlocal — ThreadLocal (6) + InheritableThreadLocal (5), 11 triples.
    //
    // W8-C3-1 left these out because "the interesting question is concurrent
    // and this file is single-threaded". Half of that is right: the MEMORY
    // semantics need a concurrent vector. But the per-thread STORAGE contract
    // is deterministic with one child thread and a join, and it is the half
    // that a Rust body backed by a process-global map gets wrong — a
    // process-global cache that ignores the thread is the recorded [vmscope]
    // defect, and nothing in the tree drives these eleven triples at all.
    //
    // initialValue() is protected, so the override in CountingTL HIDES it. The
    // registered triple is reached through superInitialValue(), which is the
    // only expression in ordinary Java source that names it.
    // ========================================================================
    static void tlocal() throws Exception {
        // -- the bare ThreadLocal: initialValue is null ------------------------
        ThreadLocal<String> bare = new ThreadLocal<>();
        ckB("tlocal:bare get() is null", bare.get() == null, true);
        bare.set("v1");
        ckS("tlocal:bare get() after set", bare.get(), "v1");
        bare.set(null);
        ckB("tlocal:bare get() after set(null)", bare.get() == null, true);
        bare.remove();
        ckB("tlocal:bare get() after remove", bare.get() == null, true);

        // -- ThreadLocal.initialValue itself, reached through super -------------
        CountingTL tl = new CountingTL();
        ckB("tlocal:super.initialValue() is null", tl.superInitialValue() == null, true);

        // -- the override, and the fact that get() consults it EXACTLY ONCE -----
        ckI("tlocal:inits before first get", tl.inits, 0);
        ckS("tlocal:first get()", tl.get(), "init");
        ckI("tlocal:inits after first get", tl.inits, 1);
        ckS("tlocal:second get()", tl.get(), "init");
        ckI("tlocal:inits after second get (must NOT have grown)", tl.inits, 1);
        tl.set("explicit");
        ckS("tlocal:get() after set", tl.get(), "explicit");
        ckI("tlocal:inits after set (must NOT have grown)", tl.inits, 1);
        // remove() must restore the INITIAL state, which means initialValue runs AGAIN.
        tl.remove();
        ckS("tlocal:get() after remove", tl.get(), "init");
        ckI("tlocal:inits after remove + get", tl.inits, 2);

        // -- withInitial -------------------------------------------------------
        ThreadLocal<String> wi = ThreadLocal.withInitial(new Sup("supplied"));
        ckS("tlocal:withInitial get()", wi.get(), "supplied");
        wi.set("over");
        ckS("tlocal:withInitial get() after set", wi.get(), "over");
        wi.remove();
        ckS("tlocal:withInitial get() after remove", wi.get(), "supplied");

        // -- InheritableThreadLocal: the value crosses into a CHILD thread ------
        CountingITL itl = new CountingITL();
        ckB("tlocal:ITL super.initialValue() is null", itl.superInitialValue() == null, true);
        ckS("tlocal:ITL parent get()", itl.get(), "parent-init");
        itl.set("from-parent");
        ckS("tlocal:ITL parent get() after set", itl.get(), "from-parent");

        final String[] seen = new String[3];
        Thread child = new Thread() {
            @Override
            public void run() {
                seen[0] = itl.get();
                itl.set("from-child");
                seen[1] = itl.get();
                itl.remove();
                seen[2] = itl.get();
            }
        };
        child.start();
        child.join();
        ckS("tlocal:ITL child inherited the parent's value", seen[0], "from-parent");
        ckS("tlocal:ITL child get() after its own set", seen[1], "from-child");
        ckS("tlocal:ITL child get() after its own remove", seen[2], "parent-init");
        ckS("tlocal:ITL parent UNCHANGED by the child's set", itl.get(), "from-parent");

        // A thread started BEFORE the parent's set must not see it: the value is copied at
        // thread construction, not read through a shared cell.
        final String[] late = new String[1];
        CountingITL fresh = new CountingITL();
        Thread early = new Thread() {
            @Override
            public void run() {
                late[0] = fresh.get();
            }
        };
        fresh.set("set-after-construction");
        early.start();
        early.join();
        ckS("tlocal:ITL value is copied at CONSTRUCTION, not read live", late[0], "parent-init");

        // A plain ThreadLocal must NOT cross into a child at all.
        final String[] plain = new String[1];
        tl.set("parent-only");
        Thread t2 = new Thread() {
            @Override
            public void run() {
                plain[0] = tl.get();
            }
        };
        t2.start();
        t2.join();
        ckS("tlocal:plain ThreadLocal does NOT cross into a child", plain[0], "init");

        sectionEnd("tlocal", 26);
    }

    // ========================================================================
    // 11. fmtobj — java.util.Formatter's 9 lifecycle triples.
    //
    // Distinct from String.format, which generation 2 already drives: these
    // are the OBJECT's lifecycle — out(), locale(), flush(), close() — where
    // the risk is state rather than grammar. The load-bearing rows are the
    // post-close ones: every method must throw FormatterClosedException, and
    // close() must be idempotent. A Rust body holding a moved-out handle
    // panics there instead.
    //
    // locale() of the no-arg constructor is the HOST's format locale, so it is
    // asserted RELATIONALLY. A stored locale name would make this fixture pass
    // only on the machine that wrote it.
    // ========================================================================
    static void fmtobj() {
        // -- new Formatter(): its own StringBuilder ----------------------------
        Formatter f = new Formatter();
        ckB("fmtobj:new Formatter().out() is a StringBuilder",
                f.out() instanceof StringBuilder, true);
        check(f.locale() != null && f.locale().equals(Locale.getDefault(Locale.Category.FORMAT)),
                "fmtobj: new Formatter().locale() must be the host's FORMAT locale, got "
                        + f.locale() + " against " + Locale.getDefault(Locale.Category.FORMAT));
        ckS("fmtobj:new Formatter().toString() when empty", f.toString(), "");
        f.format("%s=%d", "n", Integer.valueOf(7));
        ckS("fmtobj:toString() after format", f.toString(), "n=7");
        check(f.out().toString().equals(f.toString()),
                "fmtobj: toString() must be out().toString() -- the same buffer, not a copy");
        // format() returns THIS, which is what makes the call chainable.
        check(f.format(";%s", "x") == f, "fmtobj: format() must return the SAME Formatter");
        ckS("fmtobj:toString() after a chained format", f.toString(), "n=7;x");

        // -- new Formatter(Locale): the locale is the caller's, verbatim --------
        Formatter fl = new Formatter(Locale.ROOT);
        ckB("fmtobj:new Formatter(ROOT).locale() is ROOT", Locale.ROOT.equals(fl.locale()), true);
        fl.format("%,d", Integer.valueOf(1234567));
        ckS("fmtobj:ROOT-grouped output", fl.toString(), "1,234,567");
        Formatter fnull = new Formatter((Locale) null);
        ckB("fmtobj:new Formatter((Locale) null).locale() is null", fnull.locale() == null, true);
        fnull.format("%,d", Integer.valueOf(1234567));
        ckS("fmtobj:null-locale output has NO grouping separator", fnull.toString(), "1,234,567");

        // -- new Formatter(Appendable): the caller's buffer, by identity --------
        StringBuilder sb = new StringBuilder("pre:");
        Formatter fa = new Formatter(sb);
        check(fa.out() == sb, "fmtobj: out() must be the caller's Appendable, by identity");
        fa.format("%03d", Integer.valueOf(5));
        ckS("fmtobj:the caller's buffer was appended to", sb.toString(), "pre:005");
        ckS("fmtobj:Formatter(Appendable).toString()", fa.toString(), "pre:005");

        // -- flush is a no-op on a non-Flushable, and close is idempotent -------
        Throwable t = null;
        try {
            fa.flush();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:flush() on a StringBuilder-backed Formatter", t, "none");
        fa.close();
        t = null;
        try {
            fa.close();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:close() twice", t, "none");
        // Everything else must now throw, and throw the SPECIFIC class.
        t = null;
        try {
            sinkO = fa.toString();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:toString() after close", t, "java.util.FormatterClosedException");
        t = null;
        try {
            sinkO = fa.out();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:out() after close", t, "java.util.FormatterClosedException");
        t = null;
        try {
            fa.flush();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:flush() after close", t, "java.util.FormatterClosedException");
        t = null;
        try {
            sinkO = fa.format("%d", Integer.valueOf(1));
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:format() after close", t, "java.util.FormatterClosedException");
        t = null;
        try {
            sinkO = fa.locale();
        } catch (Throwable x) {
            t = x;
        }
        ckX("fmtobj:locale() after close", t, "java.util.FormatterClosedException");
        // The caller's buffer must still hold what was written before the close.
        ckS("fmtobj:the buffer survives the close", sb.toString(), "pre:005");

        sectionEnd("fmtobj", 22);
    }

    // ========================================================================
    // 12. inet — InetSocketAddress (12) + SocketAddress.toString (1).
    //
    // W8-C3-1 left these out because "resolution behaviour is host-dependent;
    // needs createUnresolved discipline throughout". That discipline is what
    // this block has: NOTHING here performs a DNS lookup. Every address is
    // either createUnresolved, a numeric literal (which InetAddress parses
    // without a resolver), the wildcard, or the loopback constant. So the
    // expected strings reproduce on a machine with no network at all, which is
    // the property that makes the family safe to register.
    //
    // The sharp rows are the port-range checks — Java throws
    // IllegalArgumentException, and a Rust body taking a u16 either panics or
    // silently truncates 65536 to 0.
    // ========================================================================
    static void inet() {
        InetSocketAddress un = InetSocketAddress.createUnresolved("example.invalid", 8080);
        ckB("inet:createUnresolved isUnresolved", un.isUnresolved(), true);
        ckB("inet:createUnresolved getAddress() is null", un.getAddress() == null, true);
        ckS("inet:createUnresolved getHostName", un.getHostName(), "example.invalid");
        ckS("inet:createUnresolved getHostString", un.getHostString(), "example.invalid");
        ckI("inet:createUnresolved getPort", un.getPort(), 8080);
        ckS("inet:createUnresolved toString", un.toString(), "example.invalid/<unresolved>:8080");

        // equals/hashCode over two independently built, equal, unresolved addresses.
        InetSocketAddress un2 = InetSocketAddress.createUnresolved(
                new String(new char[] {'e', 'x', 'a', 'm', 'p', 'l', 'e'}) + ".invalid", 8080);
        check(un != un2, "inet: the two unresolved addresses must be DISTINCT objects");
        ckB("inet:createUnresolved equals an equal one", un.equals(un2), true);
        ckB("inet:equals is symmetric", un2.equals(un), true);
        check(un.hashCode() == un2.hashCode(),
                "inet: equal InetSocketAddresses must have equal hash codes");
        ckB("inet:unresolved equals a different port",
                un.equals(InetSocketAddress.createUnresolved("example.invalid", 8081)), false);
        ckB("inet:unresolved equals a String", un.equals("example.invalid:8080"), false);

        // A RESOLVED address must never equal an unresolved one with the same host text.
        InetSocketAddress loop = new InetSocketAddress(InetAddress.getLoopbackAddress(), 8080);
        ckB("inet:loopback isUnresolved", loop.isUnresolved(), false);
        ckI("inet:loopback getPort", loop.getPort(), 8080);
        ckB("inet:loopback getAddress() is non-null", loop.getAddress() != null, true);
        ckS("inet:loopback getHostAddress", loop.getAddress().getHostAddress(), "127.0.0.1");

        // The wildcard constructor.
        InetSocketAddress wild = new InetSocketAddress(0);
        ckB("inet:wildcard isUnresolved", wild.isUnresolved(), false);
        ckI("inet:wildcard getPort", wild.getPort(), 0);
        ckB("inet:wildcard address isAnyLocalAddress", wild.getAddress().isAnyLocalAddress(), true);
        ckS("inet:wildcard getHostString", wild.getHostString(), "0.0.0.0");

        // A NUMERIC literal host reaches the (String, int) constructor without a resolver.
        InetSocketAddress numeric = new InetSocketAddress("127.0.0.2", 9000);
        ckB("inet:numeric-literal host isUnresolved", numeric.isUnresolved(), false);
        ckI("inet:numeric-literal getPort", numeric.getPort(), 9000);
        ckS("inet:numeric-literal getHostString", numeric.getHostString(), "127.0.0.2");
        ckS("inet:numeric-literal getHostAddress", numeric.getAddress().getHostAddress(), "127.0.0.2");
        ckS("inet:numeric-literal toString", numeric.toString(), "/127.0.0.2:9000");
        ckB("inet:resolved does NOT equal unresolved with the same text",
                numeric.equals(InetSocketAddress.createUnresolved("127.0.0.2", 9000)), false);

        // -- the port range, on every constructor that takes one ---------------
        Throwable t = null;
        try {
            sinkO = new InetSocketAddress(65536);
        } catch (Throwable x) {
            t = x;
        }
        ckX("inet:new InetSocketAddress(65536)", t, "java.lang.IllegalArgumentException");
        t = null;
        try {
            sinkO = new InetSocketAddress(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("inet:new InetSocketAddress(-1)", t, "java.lang.IllegalArgumentException");
        t = null;
        try {
            sinkO = InetSocketAddress.createUnresolved("h", 65536);
        } catch (Throwable x) {
            t = x;
        }
        ckX("inet:createUnresolved(h, 65536)", t, "java.lang.IllegalArgumentException");
        t = null;
        try {
            sinkO = InetSocketAddress.createUnresolved(null, 80);
        } catch (Throwable x) {
            t = x;
        }
        ckX("inet:createUnresolved(null, 80)", t, "java.lang.IllegalArgumentException");
        t = null;
        try {
            sinkO = new InetSocketAddress(InetAddress.getLoopbackAddress(), 65536);
        } catch (Throwable x) {
            t = x;
        }
        ckX("inet:new InetSocketAddress(addr, 65536)", t, "java.lang.IllegalArgumentException");
        // 65535 is the LAST legal port; a body that rejects it has an off-by-one.
        ckI("inet:port 65535 is legal", new InetSocketAddress(65535).getPort(), 65535);

        // -- SocketAddress.toString, from a SocketAddress-typed call site -------
        SocketAddress sa = un;
        ckS("inet:SocketAddress-typed toString", sa.toString(), "example.invalid/<unresolved>:8080");
        check(sa.toString().equals(un.toString()),
                "inet: the SocketAddress-typed call must dispatch to the receiver's runtime"
                        + " class, so it must answer identically to the InetSocketAddress-typed"
                        + " one -- a route discriminator that needs no stored value");

        sectionEnd("inet", 34);
    }

    // ========================================================================
    // 13. regex — Pattern.compile x2, Matcher x8, Scanner x3.
    //
    // "A regex engine deserves its own differential" — it does, and this is not
    // it. What this block covers is the part a differential over MATCH RESULTS
    // would miss anyway: the group INDEX bounds (hazard 2, and Java's exact
    // class is IndexOutOfBoundsException, not ArrayIndexOutOfBounds), the
    // ILLEGAL-STATE protocol (group() before find() is IllegalStateException,
    // not a null), and the fact that an unmatched optional group is a NULL
    // string with start()/end() of -1 rather than an empty one.
    // ========================================================================
    static void regex() {
        Pattern p = Pattern.compile("(a+)(b+)?");
        ckI("regex:compile groupCount", p.matcher("").groupCount(), 2);
        ckS("regex:compile pattern text", p.pattern(), "(a+)(b+)?");

        Matcher m = p.matcher("xxaaabbyyaa");

        // -- the illegal-state protocol, BEFORE any search ---------------------
        Throwable t = null;
        try {
            sinkO = m.group();
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:group() before find()", t, "java.lang.IllegalStateException");
        t = null;
        try {
            sinkI = m.start();
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:start() before find()", t, "java.lang.IllegalStateException");
        t = null;
        try {
            sinkI = m.end();
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:end() before find()", t, "java.lang.IllegalStateException");

        // -- the first match ---------------------------------------------------
        ckB("regex:first find()", m.find(), true);
        ckS("regex:group()", m.group(), "aaabb");
        ckS("regex:group(0)", m.group(0), "aaabb");
        ckS("regex:group(1)", m.group(1), "aaa");
        ckS("regex:group(2)", m.group(2), "bb");
        ckI("regex:start()", m.start(), 2);
        ckI("regex:end()", m.end(), 7);
        ckI("regex:start(1)", m.start(1), 2);
        ckI("regex:end(2)", m.end(2), 7);

        // -- the second match, where group 2 does NOT participate ---------------
        ckB("regex:second find()", m.find(), true);
        ckS("regex:second group()", m.group(), "aa");
        ckB("regex:unmatched group(2) is NULL, not empty", m.group(2) == null, true);
        ckI("regex:unmatched start(2)", m.start(2), -1);
        ckI("regex:unmatched end(2)", m.end(2), -1);
        ckB("regex:third find()", m.find(), false);

        // -- group index bounds: the EXACT class, on both sides ------------------
        m.reset();
        ckB("regex:find() after reset", m.find(), true);
        t = null;
        try {
            sinkO = m.group(99);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:group(99)", t, "java.lang.IndexOutOfBoundsException");
        t = null;
        try {
            sinkO = m.group(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:group(-1)", t, "java.lang.IndexOutOfBoundsException");
        t = null;
        try {
            sinkI = m.start(99);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:start(99)", t, "java.lang.IndexOutOfBoundsException");
        t = null;
        try {
            sinkI = m.end(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:end(-1)", t, "java.lang.IndexOutOfBoundsException");

        // -- find(int): the RESET-and-search overload ----------------------------
        ckB("regex:find(8)", m.find(8), true);
        ckI("regex:start() after find(8)", m.start(), 9);
        ckB("regex:find(11) past the last match", m.find(11), false);
        t = null;
        try {
            sinkZ = m.find(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:find(-1)", t, "java.lang.IndexOutOfBoundsException");
        t = null;
        try {
            sinkZ = m.find(99);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:find(99) past the end of the input", t, "java.lang.IndexOutOfBoundsException");

        // -- compile(String, int): the flags overload is a SEPARATE triple --------
        Pattern ci = Pattern.compile("ABC", Pattern.CASE_INSENSITIVE);
        ckB("regex:CASE_INSENSITIVE matches lowercase", ci.matcher("abc").matches(), true);
        ckB("regex:the no-flags twin does NOT", Pattern.compile("ABC").matcher("abc").matches(),
                false);
        ckI("regex:compile(s, flags).flags()", ci.flags(), 2);
        t = null;
        try {
            sinkO = Pattern.compile("(");
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:compile(\"(\")", t, "java.util.regex.PatternSyntaxException");
        t = null;
        try {
            sinkO = Pattern.compile("(", 0);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:compile(\"(\", 0)", t, "java.util.regex.PatternSyntaxException");

        // -- Scanner: findWithinHorizon x2 and match ------------------------------
        Scanner sc = new Scanner("hello 42 world");
        t = null;
        try {
            sinkO = sc.match();
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:Scanner.match() before any search", t, "java.lang.IllegalStateException");
        ckS("regex:findWithinHorizon(String, 0 = unbounded)",
                sc.findWithinHorizon("\\d+", 0), "42");
        ckS("regex:match() after findWithinHorizon", sc.match().group(), "42");
        ckI("regex:match().start()", sc.match().start(), 6);

        Scanner sc2 = new Scanner("hello 42 world");
        ckS("regex:findWithinHorizon(Pattern, 5) cannot reach the digits",
                sc2.findWithinHorizon(Pattern.compile("\\d+"), 5), null);
        ckS("regex:findWithinHorizon(Pattern, 0) can", sc2.findWithinHorizon(
                Pattern.compile("\\d+"), 0), "42");
        t = null;
        try {
            sinkO = sc2.findWithinHorizon("x", -1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("regex:findWithinHorizon(s, -1)", t, "java.lang.IllegalArgumentException");

        // Scanner over a READABLE, not a String. Every Scanner row above uses
        // the String constructor, which is why an empty Scanner(Reader) was
        // invisible: `new Scanner(System.in)`, `new Scanner(new FileReader(f))`
        // and `new Scanner(new InputStreamReader(s))` all take this path.
        Scanner sr = new Scanner(new StringReader("hello 42 world"));
        ckB("regex:Scanner(Reader).hasNext()", sr.hasNext(), true);
        ckS("regex:Scanner(Reader).next()", sr.next(), "hello");
        ckI("regex:Scanner(Reader).nextInt()", sr.nextInt(), 42);

        // Longer than one read chunk, so a chunked drain that stops after the
        // first block is caught. 5000 'a' then a space then a token.
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 5000; i++) {
            big.append('a');
        }
        big.append(" tail");
        Scanner sr2 = new Scanner(new StringReader(big.toString()));
        ckI("regex:Scanner(Reader) first token spans chunks", sr2.next().length(), 5000);
        ckS("regex:Scanner(Reader) token after the chunk boundary", sr2.next(), "tail");

        sectionEnd("regex", 47);
    }

    // ========================================================================
    // 14. misc — five singletons whose classes have no other registered member.
    //
    // Each is one triple in a class the rest of which is not registered, so
    // none of them justifies a family. They are here because the alternative
    // is that they stay at zero callers forever.
    // ========================================================================
    public static class MiscHolder {
        public MiscHolder() { }

        public MiscHolder(int a) { }

        public int takesInt(int a) {
            return a;
        }

        public void m(int i) {}
    }

    abstract static class MiscAbstract {
        MiscAbstract() {}
    }

    static void misc() {
        // -- String(StringBuilder): a SNAPSHOT, not a view ---------------------
        StringBuilder sb = new StringBuilder("abc");
        String snap = new String(sb);
        ckS("misc:new String(StringBuilder)", snap, "abc");
        sb.append("def");
        ckS("misc:the snapshot did not follow the builder", snap, "abc");
        ckS("misc:the builder moved on", sb.toString(), "abcdef");
        // An empty builder, and one holding a LONE SURROGATE that no Rust `str` can carry.
        ckI("misc:new String(empty StringBuilder).length", new String(new StringBuilder()).length(),
                0);
        StringBuilder lone = new StringBuilder();
        lone.append((char) 0xdc00);
        String loneStr = new String(lone);
        ckI("misc:new String(builder holding U+DC00).length", loneStr.length(), 1);
        ckI("misc:new String(builder holding U+DC00).charAt(0)", loneStr.charAt(0), 56320);

        // -- PrintStream.charset(): the stream's OWN charset, not the host's ----
        PrintStream ps = new PrintStream(new ByteArrayOutputStream(), true,
                StandardCharsets.UTF_16BE);
        ckS("misc:PrintStream(UTF-16BE).charset().name()", ps.charset().name(), "UTF-16BE");
        PrintStream ps2 = new PrintStream(new ByteArrayOutputStream(), true,
                StandardCharsets.ISO_8859_1);
        ckS("misc:PrintStream(ISO-8859-1).charset().name()", ps2.charset().name(), "ISO-8859-1");
        check(!ps.charset().equals(ps2.charset()),
                "misc: two PrintStreams built with DIFFERENT charsets must report different"
                        + " ones -- a body answering the host default gives the same twice");

        // -- EnumMap(Class): key type is fixed at construction -------------------
        EnumMap<RoundingMode, String> em = new EnumMap<>(RoundingMode.class);
        ckI("misc:new EnumMap(Class).size()", em.size(), 0);
        ckB("misc:new EnumMap(Class).isEmpty()", em.isEmpty(), true);
        em.put(RoundingMode.HALF_EVEN, "he");
        em.put(RoundingMode.CEILING, "c");
        ckI("misc:EnumMap size after two puts", em.size(), 2);
        // EnumMap iterates in ORDINAL order regardless of insertion order.
        ckS("misc:EnumMap iterates in ORDINAL order", em.toString(), "{CEILING=c, HALF_EVEN=he}");
        Throwable t = null;
        try {
            sinkO = new EnumMap<RoundingMode, String>((Class<RoundingMode>) null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:new EnumMap((Class) null)", t, "java.lang.NullPointerException");

        // -- Iterator.remove(): the illegal-state protocol -----------------------
        List<String> list = new ArrayList<>(Arrays.asList("a", "b", "c"));
        Iterator<String> it = list.iterator();
        t = null;
        try {
            it.remove();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Iterator.remove() before next()", t, "java.lang.IllegalStateException");
        ckS("misc:first next()", it.next(), "a");
        it.remove();
        ckS("misc:the list after removing the first element", list.toString(), "[b, c]");
        t = null;
        try {
            it.remove();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Iterator.remove() twice in a row", t, "java.lang.IllegalStateException");
        ckS("misc:the list is unchanged by the failed second remove", list.toString(), "[b, c]");

        // -- DateFormat.format(Date): the intercepted triple, on a FIXED zone -----
        // RSimpleDateFormatZone owns this defect in depth; this row exists so the triple has a
        // caller inside the census too, and it is deliberately the ZoneInfo case that block's
        // control family says must stay correct.
        SimpleDateFormat sdf = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss Z", Locale.US);
        sdf.setTimeZone(TimeZone.getTimeZone("Asia/Kolkata"));
        DateFormat df = sdf;
        ckS("misc:DateFormat.format(Date) on Asia/Kolkata", df.format(new Date(1610712000000L)),
                "2021-01-15 17:30:00 +0530");
        check(df.format(new Date(1610712000000L)).equals(sdf.format(new Date(1610712000000L))),
                "misc: the DateFormat-typed and SimpleDateFormat-typed call sites must agree");

        // -- File.createTempFile validates its prefix -------------------------
        // A VALIDATION contract, not a message one: this VM created the file.
        t = null;
        try {
            sinkO = File.createTempFile("m", ".t");
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:createTempFile short prefix", t, "java.lang.IllegalArgumentException");
        ckS("misc:createTempFile short prefix message", t == null ? null : t.getMessage(),
                "Prefix string \"m\" too short: length must be at least 3");
        t = null;
        try {
            sinkO = File.createTempFile(null, ".t");
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:createTempFile null prefix", t, "java.lang.NullPointerException");

        // -- NoSuchMethodException names the class AND the signature -----------
        // The separator is a bare comma and an array renders as getName()
        // (`[I`), not `int[]`; both were measured after being guessed wrong.
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("nope");
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:NoSuchMethodException no-arg message", t == null ? null : t.getMessage(),
                "RJdkIntrinsics3$MiscHolder.nope()");
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("nope", int.class, String.class);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:NoSuchMethodException signature message", t == null ? null : t.getMessage(),
                "RJdkIntrinsics3$MiscHolder.nope(int,java.lang.String)");
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("nope", int[].class);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:NoSuchMethodException array parameter renders as getName", 
                t == null ? null : t.getMessage(),
                "RJdkIntrinsics3$MiscHolder.nope([I)");

        // -- InstantiationException is a TYPE, not a word in a message ---------
        // catch (InstantiationException) must match; the message is null for an
        // abstract class and getName() for an interface.
        t = null;
        try {
            sinkO = MiscAbstract.class.newInstance();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.newInstance on abstract", t, "java.lang.InstantiationException");
        ckS("misc:Class.newInstance on abstract has a null message",
                t == null ? "no throw" : String.valueOf(t.getMessage()), "null");
        t = null;
        try {
            sinkO = Runnable.class.newInstance();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.newInstance on interface", t, "java.lang.InstantiationException");
        ckS("misc:Class.newInstance on interface names it", t == null ? null : t.getMessage(),
                "java.lang.Runnable");

        // A PRIMITIVE mirror carries no class id -- there is no `int` class to
        // resolve -- and that was reported as "receiver is not a Class mirror".
        // It is one: `int.class` is a Class, and the answer is the same
        // InstantiationException every other uninstantiable type gets.
        t = null;
        try {
            sinkO = int.class.newInstance();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.newInstance on int.class", t, "java.lang.InstantiationException");
        ckS("misc:Class.newInstance on int.class names the primitive",
                t == null ? null : t.getMessage(), "int");
        t = null;
        try {
            sinkO = void.class.newInstance();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.newInstance on void.class", t, "java.lang.InstantiationException");
        ckS("misc:Class.newInstance on void.class names it",
                t == null ? null : t.getMessage(), "void");

        // -- a negative timeout is an error, not a long wait ------------------
        // `o.wait(-5)` on a held monitor USED TO BLOCK FOREVER: the arm folded
        // `ms <= 0` into "wait forever" and the thread parked with nobody to
        // notify it. If that regresses, this family HANGS rather than failing,
        // and the harness reports a 120s timeout — which is the correct and
        // only possible signal for a liveness defect.
        //
        // Zero really does mean forever (JLS 17.2), so the contract is `< 0`.
        final Object mon = new Object();
        t = null;
        try {
            Thread.sleep(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Thread.sleep(-1)", t, "java.lang.IllegalArgumentException");
        ckS("misc:Thread.sleep(-1) message", t == null ? null : t.getMessage(),
                "timeout value is negative");
        t = null;
        try {
            synchronized (mon) {
                mon.wait(-5);
            }
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Object.wait(-5) refuses instead of hanging", t,
                "java.lang.IllegalArgumentException");
        ckS("misc:Object.wait(-5) message", t == null ? null : t.getMessage(),
                "timeout value is negative");
        // The two overloads do NOT use the same noun. Measured, not composed.
        t = null;
        try {
            synchronized (mon) {
                mon.wait(-5, 0);
            }
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:Object.wait(-5,0) says timeoutMillis, not timeout",
                t == null ? null : t.getMessage(), "timeoutMillis value is negative");
        // Zero is still an indefinite wait, so it must NOT be refused — asserted
        // by a notify from another thread rather than by waiting for one.
        t = null;
        try {
            synchronized (mon) {
                mon.wait(1);
            }
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Object.wait(1) is a real timed wait, not a refusal", t, "none");

        // -- the messages around it -------------------------------------------
        t = null;
        try {
            mon.wait();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:wait() without the monitor", t, "java.lang.IllegalMonitorStateException");
        ckS("misc:wait() without the monitor message", t == null ? null : t.getMessage(),
                "current thread is not owner");
        t = null;
        try {
            Thread th = new Thread(() -> { });
            th.start();
            th.join();
            th.start();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Thread.start() twice", t, "java.lang.IllegalThreadStateException");
        ckS("misc:Thread.start() twice has a NULL message",
                t == null ? "no throw" : String.valueOf(t.getMessage()), "null");
        t = null;
        try {
            Thread.currentThread().interrupt();
            Thread.sleep(1);
        } catch (Throwable x) {
            t = x;
        } finally {
            Thread.interrupted();
        }
        ckX("misc:sleep after interrupt", t, "java.lang.InterruptedException");
        ckS("misc:sleep after interrupt names the operation",
                t == null ? null : t.getMessage(), "sleep interrupted");
        t = null;
        try {
            sinkO = Class.forName(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.forName(null)", t, "java.lang.NullPointerException");
        ckS("misc:Class.forName(null) has a NULL message",
                t == null ? "no throw" : String.valueOf(t.getMessage()), "null");

        // -- what a reflective CALL says when it refuses ----------------------
        // Sweep 10 (scratchpad/g75/M.java, 31 rows): 23 were already exact,
        // including all the InvocationTargetException wrapping, the access
        // checks, widening/narrowing, and the whole java.lang.reflect.Array
        // family. These are the ones that were not.
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("takesInt", int.class).invoke(new MiscHolder());
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Method.invoke with too few arguments", t,
                "java.lang.IllegalArgumentException");
        // Bare, and the counts run GOT then EXPECTED — the opposite order to
        // how anyone writes it. Ours named the class and method, which is more
        // useful and is not what a caller matching the text sees.
        ckS("misc:Method.invoke arity message", t == null ? null : t.getMessage(),
                "wrong number of arguments: 0 expected: 1");
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("takesInt", int.class)
                    .invoke(new MiscHolder(), 1, 2);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:Method.invoke arity message counts the SURPLUS too",
                t == null ? null : t.getMessage(), "wrong number of arguments: 2 expected: 1");
        // Constructor uses the SAME sentence — HotSpot does not distinguish.
        t = null;
        try {
            sinkO = MiscHolder.class.getConstructor(int.class).newInstance();
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:Constructor.newInstance arity message is the same sentence",
                t == null ? null : t.getMessage(), "wrong number of arguments: 0 expected: 1");

        // A null receiver is HotSpot's helpful NPE, naming the JDK's own local.
        // NOTE the variable is `obj` here and `o` in Field.set — two call sites,
        // two names, neither derivable from the other.
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("takesInt", int.class).invoke(null, 1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Method.invoke with a null receiver", t, "java.lang.NullPointerException");
        ckS("misc:Method.invoke null-receiver message", t == null ? null : t.getMessage(),
                "Cannot invoke \"Object.getClass()\" because \"obj\" is null");

        // The refusal carries a CAUSE, and frameworks branch on it: Spring's
        // InvocableHandlerMethod tests `getCause() instanceof NPE`. Method.invoke
        // attached one and Constructor.newInstance dropped it, so the same bad
        // argument produced different exceptions through the two doors.
        t = null;
        try {
            sinkO = MiscHolder.class.getMethod("takesInt", int.class)
                    .invoke(new MiscHolder(), (Object) null);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:Method.invoke null-into-primitive cause",
                t == null || t.getCause() == null ? "none" : t.getCause().getClass().getName(),
                "java.lang.NullPointerException");
        t = null;
        try {
            sinkO = MiscHolder.class.getConstructor(int.class).newInstance((Object) null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Constructor.newInstance null-into-primitive", t,
                "java.lang.IllegalArgumentException");
        ckS("misc:Constructor.newInstance carries the SAME cause as Method.invoke",
                t == null || t.getCause() == null ? "none" : t.getCause().getClass().getName(),
                "java.lang.NullPointerException");

        // -- an array's class is not its component's --------------------------
        // Sweep 11 (scratchpad/g76/A.java, 51 rows) audited G69-1 N1's claim
        // that `class_id_of_object(array)` answers the COMPONENT's id. Array
        // identity turned out to be in good shape — 50 of 51 exact, including
        // getName/getSimpleName/getCanonicalName across dimensions, component
        // types, assignability, forName round trips and reflect.Array. THIS is
        // the one live site: the cast refusal named the component.
        t = null;
        try {
            sinkO = Object[].class.cast(new int[] {1});
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Class.cast of an int[] to Object[]", t, "java.lang.ClassCastException");
        ckS("misc:the cast refusal names the ARRAY, not its component",
                t == null ? null : t.getMessage(), "Cannot cast [I to [Ljava.lang.Object;");
        // The control that makes the row above mean something: a non-array
        // receiver was always right, so the fix is about arrays specifically.
        t = null;
        try {
            sinkO = Integer.class.cast("s");
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:a non-array cast refusal was already correct",
                t == null ? null : t.getMessage(),
                "Cannot cast java.lang.String to java.lang.Integer");

        // -- what a failed PARSE says, and what a bad RANGE says ---------------
        // Sweep 13 (scratchpad/g78/N.java, 48 rows): 42 already exact — every
        // Integer/Long/radix/BigInteger/BigDecimal message, stream-after-close
        // semantics and mark/reset. These are the six that were not.
        t = null;
        try {
            sinkD = Double.parseDouble("");
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Double.parseDouble(\"\")", t, "java.lang.NumberFormatException");
        ckS("misc:the float parsers say 'empty String'", t == null ? null : t.getMessage(),
                "empty String");
        // ...and a BLANK string is empty too — the JDK trims first.
        t = null;
        try {
            sinkD = Double.parseDouble("   ");
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:a blank float parse is also 'empty String'",
                t == null ? null : t.getMessage(), "empty String");
        // The CONTROL that makes the two rows above mean something: the
        // INTEGRAL parsers do not share the message, so it cannot be hoisted.
        t = null;
        try {
            sinkI = Integer.parseInt("");
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:the integral parsers do NOT say 'empty String'",
                t == null ? null : t.getMessage(), "For input string: \"\"");
        t = null;
        try {
            sinkF = Float.parseFloat(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:Float.parseFloat(null)", t, "java.lang.NullPointerException");
        ckS("misc:the null float parse names the JDK's own local",
                t == null ? null : t.getMessage(),
                "Cannot invoke \"String.length()\" because \"in\" is null");

        // EOFException carries NO message; "Unexpected EOF" was ours and reads
        // like a JDK string, which is what kept it.
        t = null;
        try {
            sinkI = new DataInputStream(new ByteArrayInputStream(new byte[] {1})).readInt();
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:DataInputStream.readInt past the end", t, "java.io.EOFException");
        ckS("misc:EOFException has a NULL message",
                t == null ? "no throw" : String.valueOf(t.getMessage()), "null");

        // Objects.checkFromIndexSize: the BASE IndexOutOfBoundsException, and
        // one message format for every failure mode — including a negative
        // length, which prints verbatim INSIDE the range rather than alone.
        t = null;
        try {
            sinkI = new ByteArrayInputStream(new byte[] {1}).read(new byte[2], 5, 1);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:a bad read range is the BASE IndexOutOfBoundsException",
                t == null ? null : t.getClass().getName(), "java.lang.IndexOutOfBoundsException");
        ckS("misc:the range message names the range and the length",
                t == null ? null : t.getMessage(), "Range [5, 5 + 1) out of bounds for length 2");
        t = null;
        try {
            sinkI = new ByteArrayInputStream(new byte[] {1}).read(new byte[2], 0, -1);
        } catch (Throwable x) {
            t = x;
        }
        ckS("misc:a NEGATIVE length prints inside the range, not on its own",
                t == null ? null : t.getMessage(), "Range [0, 0 + -1) out of bounds for length 2");

        // A validation gap, not a message one: this used to SUCCEED.
        t = null;
        try {
            sinkO = new ByteArrayOutputStream(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:new ByteArrayOutputStream(-1)", t, "java.lang.IllegalArgumentException");
        ckS("misc:negative capacity message", t == null ? null : t.getMessage(),
                "Negative initial size: -1");
        // Zero is LEGAL — the control that stops the guard becoming `<= 0`.
        t = null;
        try {
            sinkO = new ByteArrayOutputStream(0);
        } catch (Throwable x) {
            t = x;
        }
        ckX("misc:new ByteArrayOutputStream(0) is legal", t, "none");

        sectionEnd("misc", 75);
    }

    // ========================================================================
    // 15. bufslice — HAZARD 2. The indexed buffer accessors, 7 triples.
    //
    // Rust panics on an out-of-bounds index; Java throws, and throws a
    // SPECIFIC class that differs BETWEEN THE THREE OPERATIONS on the same
    // object: CharBuffer.put past the limit is BufferOverflowException,
    // get() past the limit is BufferUnderflowException, and charAt(int) out of
    // range is IndexOutOfBoundsException. A body that funnels all three into
    // one class is wrong in a way an `instanceof Exception` test cannot see,
    // so every row asserts the exact class name. ([subcls!=cls].)
    //
    // Every call from here on is a candidate to abort the VM rather than
    // throw, so each is preceded by its own step marker: on a VM that panics,
    // the last line on stdout names the call that killed it.
    // ========================================================================
    static void bufslice() {
        // The two endian views are SEPARATE registered classes reached only through
        // asCharBuffer() on a ByteBuffer of the corresponding order.
        ByteBuffer beb = ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN);
        beb.put(0, (byte) 0x00).put(1, (byte) 0x41).put(2, (byte) 0x00).put(3, (byte) 0x42);
        CharBuffer be = beb.asCharBuffer();
        ByteBuffer leb = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        leb.put(0, (byte) 0x41).put(1, (byte) 0x00).put(2, (byte) 0x42).put(3, (byte) 0x00);
        CharBuffer le = leb.asCharBuffer();

        ckS("bufslice:big-endian view class", be.getClass().getSimpleName(), "ByteBufferAsCharBufferB");
        ckS("bufslice:little-endian view class", le.getClass().getSimpleName(), "ByteBufferAsCharBufferL");
        ckI("bufslice:view capacity over an 8-byte ByteBuffer", be.capacity(), 4);

        // -- get(): relative, and it ADVANCES the position -----------------------
        step("bufslice", "ByteBufferAsCharBufferB.get()");
        ckI("bufslice:BE get() first", be.get(), 65);
        ckI("bufslice:BE position after one get()", be.position(), 1);
        ckI("bufslice:BE get() second", be.get(), 66);
        step("bufslice", "ByteBufferAsCharBufferL.get()");
        ckI("bufslice:LE get() first", le.get(), 65);
        ckI("bufslice:LE get() second", le.get(), 66);
        // The SAME bytes read through the two orders must give DIFFERENT chars; a view that
        // ignores the byte order answers the same for both.
        CharBuffer be2 = ByteBuffer.wrap(new byte[] {0x12, 0x34}).order(ByteOrder.BIG_ENDIAN)
                .asCharBuffer();
        CharBuffer le2 = ByteBuffer.wrap(new byte[] {0x12, 0x34}).order(ByteOrder.LITTLE_ENDIAN)
                .asCharBuffer();
        ckI("bufslice:the same two bytes, BE", be2.get(), 4660);
        ckI("bufslice:the same two bytes, LE", le2.get(), 13330);

        // -- get() past the limit ------------------------------------------------
        be.position(be.limit());
        Throwable t = null;
        step("bufslice", "ByteBufferAsCharBufferB.get() at the limit");
        try {
            sinkC = be.get();
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:BE get() at the limit", t, "java.nio.BufferUnderflowException");
        le.position(le.limit());
        t = null;
        step("bufslice", "ByteBufferAsCharBufferL.get() at the limit");
        try {
            sinkC = le.get();
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:LE get() at the limit", t, "java.nio.BufferUnderflowException");

        // -- charAt(int): RELATIVE to the position, which is the whole trap -------
        le.position(1);
        step("bufslice", "ByteBufferAsCharBufferL.charAt(0)");
        ckI("bufslice:LE charAt(0) at position 1", le.charAt(0), 66);
        ckI("bufslice:LE position unchanged by charAt", le.position(), 1);
        le.position(0);
        step("bufslice", "ByteBufferAsCharBufferL.charAt(0) at position 0");
        ckI("bufslice:LE charAt(0) at position 0", le.charAt(0), 65);
        t = null;
        step("bufslice", "ByteBufferAsCharBufferL.charAt(-1)");
        try {
            sinkC = le.charAt(-1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:LE charAt(-1)", t, "java.lang.IndexOutOfBoundsException");
        t = null;
        step("bufslice", "ByteBufferAsCharBufferL.charAt(remaining())");
        try {
            sinkC = le.charAt(le.remaining());
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:LE charAt(remaining())", t, "java.lang.IndexOutOfBoundsException");

        // -- hasArray() must be FALSE: a view over a ByteBuffer has no char[] ------
        step("bufslice", "ByteBufferAsCharBufferL.hasArray()");
        ckB("bufslice:LE hasArray()", le.hasArray(), false);
        t = null;
        try {
            sinkO = le.array();
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:LE array() when hasArray() is false", t, "java.lang.UnsupportedOperationException");

        // -- toString(): the REMAINING chars, not the whole buffer -----------------
        le.position(0);
        step("bufslice", "ByteBufferAsCharBufferL.toString()");
        ckS("bufslice:LE toString() from position 0", le.toString(), "AB\u0000\u0000");
        le.position(1);
        ckS("bufslice:LE toString() from position 1", le.toString(), "B\u0000\u0000");
        ckI("bufslice:LE toString().length() equals remaining()", le.toString().length(), 3);

        // -- CharBuffer.put(char): overflow and read-only --------------------------
        CharBuffer w = CharBuffer.allocate(2);
        step("bufslice", "CharBuffer.put(char)");
        check(w.put('a') == w, "bufslice: put(char) must return THIS buffer");
        w.put('b');
        ckI("bufslice:position after two puts", w.position(), 2);
        t = null;
        step("bufslice", "CharBuffer.put(char) past the limit");
        try {
            sinkO = w.put('c');
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:put(char) past the limit", t, "java.nio.BufferOverflowException");
        w.flip();
        ckS("bufslice:the buffer after flip", w.toString(), "ab");
        CharBuffer ro = CharBuffer.allocate(2).asReadOnlyBuffer();
        t = null;
        step("bufslice", "CharBuffer.put(char) on a read-only buffer");
        try {
            sinkO = ro.put('a');
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:put(char) on a read-only buffer", t, "java.nio.ReadOnlyBufferException");
        t = null;
        step("bufslice", "ByteBufferAsCharBuffer.put(char) on a read-only view");
        try {
            sinkO = ByteBuffer.allocate(4).asReadOnlyBuffer().asCharBuffer().put('a');
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:put(char) on a read-only char VIEW", t, "java.nio.ReadOnlyBufferException");

        // -- Base64.Decoder.decode(byte[]) -----------------------------------------
        step("bufslice", "Base64.getDecoder().decode(byte[])");
        byte[] dec = Base64.getDecoder().decode("QUJD".getBytes(StandardCharsets.US_ASCII));
        ckS("bufslice:decode(byte[]) result", Arrays.toString(dec), "[65, 66, 67]");
        ckI("bufslice:decode(byte[]) of an empty array length",
                Base64.getDecoder().decode(new byte[0]).length, 0);
        // Missing padding is ACCEPTED by the basic decoder; the wrong alphabet is not.
        ckS("bufslice:decode(byte[]) without padding",
                Arrays.toString(Base64.getDecoder().decode("QQ".getBytes(
                        StandardCharsets.US_ASCII))), "[65]");
        t = null;
        step("bufslice", "Base64.getDecoder().decode(byte[]) with the URL alphabet");
        try {
            sinkO = Base64.getDecoder().decode("-_-_".getBytes(StandardCharsets.US_ASCII));
        } catch (Throwable x) {
            t = x;
        }
        ckX("bufslice:decode(byte[]) of the URL alphabet", t, "java.lang.IllegalArgumentException");
        // The byte[] overload must agree with the String overload generation 2 already drives.
        check(Arrays.equals(Base64.getDecoder().decode("QUJD".getBytes(StandardCharsets.US_ASCII)),
                        Base64.getDecoder().decode("QUJD")),
                "bufslice: decode(byte[]) and decode(String) are separate registered triples and"
                        + " must agree -- a route discriminator needing no stored value");

        sectionEnd("bufslice", 33);
    }

    // ========================================================================
    // 16. mathexact — HAZARD 1, and the family most likely to abort the VM.
    //
    // 17 triples: the twelve *Exact forms, toIntExact, and the four integral
    // max/min.
    //
    // Rust checks integer overflow in arithmetic only in debug, but it checks
    // DIVISION overflow unconditionally, and neither `panic!` nor an
    // `unwrap()` on a `checked_*` is a Java throwable: it terminates the VM
    // rather than unwinding to the catch block three lines below. Java's rule
    // is that every method here THROWS ArithmeticException with a specified
    // message ("integer overflow", "long overflow"), and that the plain
    // operators WRAP.
    //
    // The non-overflowing rows all run FIRST, so a VM that dies has already
    // reported that the ordinary path works, and each overflowing call is
    // preceded by its own step marker naming it.
    // ========================================================================
    static void mathexact() {
        // -- max/min on the integral types. No overflow hazard; they go first. ---
        ckI("mathexact:Math.max(MIN,MAX)", Math.max(iMin, iMax), 2147483647);
        ckI("mathexact:Math.min(MIN,MAX)", Math.min(iMin, iMax), -2147483648);
        ckI("mathexact:Math.max(-1,0)", Math.max(iNegOne, iZero), 0);
        ckI("mathexact:Math.min(-1,0)", Math.min(iNegOne, iZero), -1);
        ckI("mathexact:Math.max(5,5)", Math.max(iFive, iFive), 5);
        ckJ("mathexact:Math.max(MIN_LONG,MAX_LONG)", Math.max(jMin, jMax), 9223372036854775807L);
        ckJ("mathexact:Math.min(MIN_LONG,MAX_LONG)", Math.min(jMin, jMax), -9223372036854775808L);
        ckJ("mathexact:Math.max(-1L,0L)", Math.max(jNegOne, jZero), 0L);
        ckJ("mathexact:Math.min(0L,0L)", Math.min(jZero, jZero), 0L);

        // -- the *Exact forms on inputs that do NOT overflow ---------------------
        ckI("mathexact:addExact(2,3)", Math.addExact(iTwo, iThree), 5);
        ckI("mathexact:addExact(MAX,-1)", Math.addExact(iMax, iNegOne), 2147483646);
        ckI("mathexact:subtractExact(2,3)", Math.subtractExact(iTwo, iThree), -1);
        ckI("mathexact:subtractExact(MIN,-1)", Math.subtractExact(iMin, iNegOne), -2147483647);
        ckI("mathexact:multiplyExact(-5,3)", Math.multiplyExact(iNegFive, iThree), -15);
        ckI("mathexact:multiplyExact(MIN,1)", Math.multiplyExact(iMin, iOne), -2147483648);
        ckI("mathexact:multiplyExact(MIN,0)", Math.multiplyExact(iMin, iZero), 0);
        ckI("mathexact:negateExact(-5)", Math.negateExact(iNegFive), 5);
        ckI("mathexact:negateExact(MAX)", Math.negateExact(iMax), -2147483647);
        ckI("mathexact:incrementExact(-1)", Math.incrementExact(iNegOne), 0);
        ckI("mathexact:incrementExact(MIN)", Math.incrementExact(iMin), -2147483647);
        ckI("mathexact:decrementExact(MAX)", Math.decrementExact(iMax), 2147483646);
        ckI("mathexact:decrementExact(0)", Math.decrementExact(iZero), -1);
        ckJ("mathexact:addExact(MAX_LONG,-1L)", Math.addExact(jMax, jNegOne), 9223372036854775806L);
        ckJ("mathexact:subtractExact(MIN_LONG,-1L)", Math.subtractExact(jMin, jNegOne), -9223372036854775807L);
        ckJ("mathexact:multiplyExact(MIN_LONG,1L)", Math.multiplyExact(jMin, jOne), -9223372036854775808L);
        ckJ("mathexact:multiplyExact(MIN_LONG,0L)", Math.multiplyExact(jMin, jZero), 0L);
        ckJ("mathexact:negateExact(MAX_LONG)", Math.negateExact(jMax), -9223372036854775807L);
        ckJ("mathexact:incrementExact(MIN_LONG)", Math.incrementExact(jMin), -9223372036854775807L);
        ckJ("mathexact:decrementExact(MAX_LONG)", Math.decrementExact(jMax), 9223372036854775806L);
        ckI("mathexact:toIntExact(5L)", Math.toIntExact(jFive), 5);
        ckI("mathexact:toIntExact(-2147483648L)", Math.toIntExact((long) iMin), -2147483648);
        ckI("mathexact:toIntExact(2147483647L)", Math.toIntExact((long) iMax), 2147483647);

        // -- the plain OPERATORS, one dispatch away, which must WRAP --------------
        check(iMin / iNegOne == Integer.MIN_VALUE,
                "mathexact: the idiv OPCODE must wrap MIN_INT / -1 to MIN_INT (JVMS 6.5)");
        check(iMin % iNegOne == 0, "mathexact: the irem OPCODE must answer 0 for MIN_INT % -1");
        check(iMin * iNegOne == Integer.MIN_VALUE, "mathexact: the imul OPCODE must wrap");
        check(-iMin == Integer.MIN_VALUE, "mathexact: the ineg OPCODE must wrap");

        // ===== from here on, every call is a candidate to ABORT the VM ==========
        Throwable t = null;
        step("mathexact", "Math.addExact(Integer.MAX_VALUE, 1)");
        try {
            sinkI = Math.addExact(iMax, iOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:addExact(MAX,1) throws", t, "java.lang.ArithmeticException");
        ckS("mathexact:addExact(MAX,1) message", t == null ? "?" : t.getMessage(), "integer overflow");

        t = null;
        step("mathexact", "Math.addExact(Integer.MIN_VALUE, -1)");
        try {
            sinkI = Math.addExact(iMin, iNegOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:addExact(MIN,-1) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.subtractExact(Integer.MIN_VALUE, 1)");
        try {
            sinkI = Math.subtractExact(iMin, iOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:subtractExact(MIN,1) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.subtractExact(Integer.MAX_VALUE, -1)");
        try {
            sinkI = Math.subtractExact(iMax, iNegOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:subtractExact(MAX,-1) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.multiplyExact(Integer.MIN_VALUE, -1)");
        try {
            sinkI = Math.multiplyExact(iMin, iNegOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:multiplyExact(MIN,-1) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.multiplyExact(65536, 65536)");
        try {
            sinkI = Math.multiplyExact(i65536, i65536);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:multiplyExact(65536,65536) throws", t, "java.lang.ArithmeticException");

        // negateExact(MIN_VALUE) is the row Rust's i32::abs/neg panics on under overflow
        // checks, and the one Java specifies as a throw.
        t = null;
        step("mathexact", "Math.negateExact(Integer.MIN_VALUE)");
        try {
            sinkI = Math.negateExact(iMin);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:negateExact(MIN) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.incrementExact(Integer.MAX_VALUE)");
        try {
            sinkI = Math.incrementExact(iMax);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:incrementExact(MAX) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.decrementExact(Integer.MIN_VALUE)");
        try {
            sinkI = Math.decrementExact(iMin);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:decrementExact(MIN) throws", t, "java.lang.ArithmeticException");

        // -- the long forms. Same rows, the OTHER registered descriptor. ----------
        t = null;
        step("mathexact", "Math.addExact(Long.MAX_VALUE, 1L)");
        try {
            sinkJ = Math.addExact(jMax, jOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:addExact(MAX_LONG,1L) throws", t, "java.lang.ArithmeticException");
        ckS("mathexact:addExact(MAX_LONG,1L) message", t == null ? "?" : t.getMessage(), "long overflow");

        t = null;
        step("mathexact", "Math.subtractExact(Long.MIN_VALUE, 1L)");
        try {
            sinkJ = Math.subtractExact(jMin, jOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:subtractExact(MIN_LONG,1L) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.multiplyExact(Long.MIN_VALUE, -1L)");
        try {
            sinkJ = Math.multiplyExact(jMin, jNegOne);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:multiplyExact(MIN_LONG,-1L) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.negateExact(Long.MIN_VALUE)");
        try {
            sinkJ = Math.negateExact(jMin);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:negateExact(MIN_LONG) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.incrementExact(Long.MAX_VALUE)");
        try {
            sinkJ = Math.incrementExact(jMax);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:incrementExact(MAX_LONG) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.decrementExact(Long.MIN_VALUE)");
        try {
            sinkJ = Math.decrementExact(jMin);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:decrementExact(MIN_LONG) throws", t, "java.lang.ArithmeticException");

        // -- toIntExact: a NARROWING that refuses rather than truncating -----------
        t = null;
        step("mathexact", "Math.toIntExact(2147483648L)");
        try {
            sinkI = Math.toIntExact(j2p31);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:toIntExact(2147483648L) throws", t, "java.lang.ArithmeticException");
        ckS("mathexact:toIntExact(2147483648L) message", t == null ? "?" : t.getMessage(), "integer overflow");

        t = null;
        step("mathexact", "Math.toIntExact(-2147483649L)");
        try {
            sinkI = Math.toIntExact(jNeg2p31m1);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:toIntExact(-2147483649L) throws", t, "java.lang.ArithmeticException");

        t = null;
        step("mathexact", "Math.toIntExact(Long.MIN_VALUE)");
        try {
            sinkI = Math.toIntExact(jMin);
        } catch (Throwable x) {
            t = x;
        }
        ckX("mathexact:toIntExact(MIN_LONG) throws", t, "java.lang.ArithmeticException");

        sectionEnd("mathexact", 57);
    }

    // @@FAMILIES@@

    // Ordered by how likely each family is to ABORT the VM rather than fail an assertion,
    // ascending. A Rust panic truncates the run, so anything after the first aborting family
    // never reports.
    static final String[] FAMILIES = {
        "objects", "boxid", "boxconv", "bitops", "strictd", "mathd", "bigdec", "bigint",
        "logrec", "tlocal", "fmtobj", "inet", "regex", "misc", "bufslice", "mathexact",
        "refmsg",
    };

    // 17. refmsg -- what a reflective FIELD refusal SAYS.
    //
    // The exception types and their precedence were already exact: 71 probe
    // rows across `scratchpad/g71/{PD,PE,PF,PG}.java` agreed with HotSpot on
    // every one, and disagreed on every message. So these rows assert TEXT, and
    // they are here because the text is the whole diagnostic: a framework that
    // fails to set a field prints this sentence and nothing else.
    //
    // Five grammars, and they are deliberately NOT uniform -- three of the rows
    // below exist only to pin an asymmetry that a tidier renderer would erase:
    //   * the bad-receiver rows carry `final`, the conversion rows do not;
    //   * the conversion rows QUOTE the field name, nothing else does;
    //   * the generic `set` names a bad RECEIVER after `to`, where every other
    //     row in that position names the value.
    // Each was measured after being got wrong or nearly guessed.
    static class RefHolder {
        static final int I = 3;
        static final long J = 4L;
        static final char C = 'a';
        static final String L = "s";
        static final int[] AR = new int[] {1};
        public int nf = 1;
        public String sref = "a";
        public final int fin = 1;
    }

    static class RefOther {
    }

    static Field rf(String n) throws Exception {
        Field x = RefHolder.class.getDeclaredField(n);
        x.setAccessible(true);
        return x;
    }

    /** The message of whatever {@code op} threw, or a marker naming what went wrong instead. */
    static String msgOf(Throwable t) {
        return t == null ? "no throw" : String.valueOf(t.getMessage());
    }

    static void refmsg() throws Exception {
        Throwable t;
        RefHolder h = new RefHolder();
        final String D = "RJdkIntrinsics3$RefHolder";
        final String O = "RJdkIntrinsics3$RefOther";

        // -- GRAMMAR 1, typed setter: the value prints as (type)value ----------
        // A LEGAL widening reports the FIELD's type and the WIDENED value, so a
        // char written into an int field prints the number, not the character.
        step("refmsg", "setInt on a static final int");
        t = null;
        try {
            rf("I").setInt(null, 9);
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:setInt on static final", t, "java.lang.IllegalAccessException");
        ckS("refmsg:setInt on static final message", msgOf(t),
                "Can not set static final int field " + D + ".I to (int)9");
        t = null;
        try {
            rf("I").setChar(null, 'z');
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:setChar into an int field widens and prints the NUMBER", msgOf(t),
                "Can not set static final int field " + D + ".I to (int)122");
        t = null;
        try {
            rf("C").setChar(null, 'z');
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:setChar into a char field prints the CHARACTER", msgOf(t),
                "Can not set static final char field " + D + ".C to (char)z");

        // An ILLEGAL widening is refused a rank EARLIER, before any conversion,
        // so it reports the SETTER's type -- and it is an IllegalArgumentException
        // where the row above is an IllegalAccessException, on the same field.
        t = null;
        try {
            rf("I").setLong(null, 9L);
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:setLong into an int field", t, "java.lang.IllegalArgumentException");
        ckS("refmsg:an ILLEGAL widening reports the SETTER's type", msgOf(t),
                "Can not set static final int field " + D + ".I to (long)9");
        t = null;
        try {
            rf("I").setFloat(null, 9f);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:setFloat prints Java's float spelling", msgOf(t),
                "Can not set static final int field " + D + ".I to (float)9.0");

        // -- GRAMMAR 1, generic setter: the value prints as its CLASS ----------
        t = null;
        try {
            rf("I").set(null, 9);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:set(Object) names the argument's CLASS", msgOf(t),
                "Can not set static final int field " + D + ".I to java.lang.Integer");
        t = null;
        try {
            rf("L").set(null, null);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:set(Object) with null says 'null value'", msgOf(t),
                "Can not set static final java.lang.String field " + D + ".L to null value");
        // An ARRAY field's type is getName() -- `[I`, not `int[]`. Reaching for
        // the `int[]` speller is the mistake G68-1 made in NoSuchMethodException.
        t = null;
        try {
            rf("AR").set(null, new int[] {2});
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:an array field and an array value both render as getName()", msgOf(t),
                "Can not set static final [I field " + D + ".AR to [I");
        t = null;
        try {
            rf("L").set(null, new String[][] {});
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:a nested array argument renders as [[L...;", msgOf(t),
                "Can not set static final java.lang.String field " + D
                        + ".L to [[Ljava.lang.String;");

        // -- rank 6: the same grammar on a NON-final field ---------------------
        // Not "argument type mismatch", which is Method.invoke's message and was
        // being applied one caller too widely.
        t = null;
        try {
            rf("nf").set(h, "x");
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:set a String into an int field", t, "java.lang.IllegalArgumentException");
        ckS("refmsg:a non-final refusal names the field too", msgOf(t),
                "Can not set int field " + D + ".nf to java.lang.String");
        t = null;
        try {
            rf("nf").set(h, null);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:null into a primitive field", msgOf(t),
                "Can not set int field " + D + ".nf to null value");

        // -- GRAMMAR 2 and 3: a bad RECEIVER prints "on"... --------------------
        // ...and DOES carry `final`, which the conversion grammar below does not.
        t = null;
        try {
            rf("fin").setInt(new RefOther(), 9);
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:typed set with a wrong receiver", t, "java.lang.IllegalArgumentException");
        ckS("refmsg:a bad receiver prints 'on' AND carries final", msgOf(t),
                "Can not set final int field " + D + ".fin on " + O);
        t = null;
        try {
            rf("fin").getInt(new RefOther());
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:a bad receiver on a getter says 'get ... on'", msgOf(t),
                "Can not get final int field " + D + ".fin on " + O);

        // ...EXCEPT the generic setter, which prints the RECEIVER after "to" --
        // in the sentence position every other rank-6 row fills with the value.
        // The argument here is a distinctive String and is still not named.
        t = null;
        try {
            rf("nf").set(new RefOther(), "ARG");
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:generic set names a bad RECEIVER after 'to', not the value", msgOf(t),
                "Can not set int field " + D + ".nf to " + O);

        // -- GRAMMAR 4: quoted name, and NO modifiers --------------------------
        // Measured on a static final field, which prints neither modifier --
        // the one grammar of the five that drops them.
        t = null;
        try {
            rf("I").getByte(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:getByte on an int field", t, "java.lang.IllegalArgumentException");
        ckS("refmsg:the conversion grammar QUOTES the name and drops the modifiers", msgOf(t),
                "Attempt to get int field \"" + D + ".I\" with illegal data type conversion to byte");
        t = null;
        try {
            rf("sref").getLong(h);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:a reference field cannot be read by a typed getter", msgOf(t),
                "Attempt to get java.lang.String field \"" + D
                        + ".sref\" with illegal data type conversion to long");

        // -- GRAMMAR 5: a null receiver is a NullPointerException with a NULL
        // message -- on four of the five entry points.
        t = null;
        try {
            rf("nf").getInt(null);
        } catch (Throwable x) {
            t = x;
        }
        ckX("refmsg:typed get with a null receiver", t, "java.lang.NullPointerException");
        ckS("refmsg:...and its message is null", msgOf(t), "null");
        t = null;
        try {
            rf("nf").get(null);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:generic get with a null receiver is also null", msgOf(t), "null");
        t = null;
        try {
            rf("nf").setInt(null, 9);
        } catch (Throwable x) {
            t = x;
        }
        ckS("refmsg:typed set with a null receiver is also null", msgOf(t), "null");

        // -- what must still SUCCEED -------------------------------------------
        // A legal widening read, and an instance final that setAccessible really
        // does unlock -- so these rows are not "reflection always throws".
        ckI("refmsg:getLong widens an int field", (int) rf("nf").getLong(h), 1);
        rf("fin").setInt(h, 41);
        ckI("refmsg:an INSTANCE final is writable after setAccessible(true)",
                rf("fin").getInt(h), 41);

        sectionEnd("refmsg", 27);
    }

    static void runFamily(String name) throws Exception {
        if ("objects".equals(name)) {
            objects();
        } else if ("boxid".equals(name)) {
            boxid();
        } else if ("boxconv".equals(name)) {
            boxconv();
        } else if ("bitops".equals(name)) {
            bitops();
        } else if ("strictd".equals(name)) {
            strictd();
        } else if ("mathd".equals(name)) {
            mathd();
        } else if ("bigdec".equals(name)) {
            bigdec();
        } else if ("bigint".equals(name)) {
            bigint();
        } else if ("logrec".equals(name)) {
            logrec();
        } else if ("tlocal".equals(name)) {
            tlocal();
        } else if ("fmtobj".equals(name)) {
            fmtobj();
        } else if ("inet".equals(name)) {
            inet();
        } else if ("regex".equals(name)) {
            regex();
        } else if ("misc".equals(name)) {
            misc();
        } else if ("bufslice".equals(name)) {
            bufslice();
        } else if ("mathexact".equals(name)) {
            mathexact();
        } else if ("refmsg".equals(name)) {
            refmsg();
        } else {
            throw new AssertionError("unknown family: " + name);
        }
    }

    public static void main(String[] args) throws Exception {
        String only = null;
        for (int k = 0; k < args.length; k++) {
            if (args[k].startsWith("--only=")) {
                only = args[k].substring("--only=".length());
            } else if ("--measure".equals(args[k])) {
                measuring = true;
            } else if ("--list".equals(args[k])) {
                for (int j = 0; j < FAMILIES.length; j++) {
                    System.out.println("CK RJdkIntrinsics3 family=" + FAMILIES[j]);
                }
                return;
            }
        }
        if (only == null) {
            for (int k = 0; k < FAMILIES.length; k++) {
                runFamily(FAMILIES[k]);
            }
        } else {
            System.out.println("CK RJdkIntrinsics3 only=" + only);
            runFamily(only);
        }
        System.out.println("CK RJdkIntrinsics3 checks=" + checks);
        System.out.println("PASS RJdkIntrinsics3 (" + checks + " checks)");
    }
}
