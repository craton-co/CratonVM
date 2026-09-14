/**
 * JDK-only corpus: {@code java.lang.StrictMath} must return the FDLIBM BITS.
 *
 * <p>{@code Math} and {@code StrictMath} have the same signatures and different
 * contracts. {@code Math.f} promises a result within 1 ULP of exact and
 * semi-monotonic, and may reach that with the host libm or a CPU intrinsic.
 * {@code StrictMath.f} promises <em>the fdlibm result, bit for bit, on every
 * platform and every VM</em> — the algorithm is normative, not just the
 * accuracy. That is the entire reason the class exists as a separate name.
 *
 * <p>So "close enough" is a defect here, and {@code Math}'s answer is not
 * automatically {@code StrictMath}'s answer. This vector therefore asserts
 * RAW BITS via {@link Double#doubleToRawLongBits}, never {@code ==} and never a
 * tolerance. A tolerance passes on platform libm and so proves nothing, which
 * is how a one-ULP {@code Random.nextGaussian} divergence survived a green
 * suite for months (W7-44-numberformat-enum-and-double-tostring.md).
 *
 * <h2>Why this file exists</h2>
 *
 * docs/known-issues/jdk-only/W7-54-strictmath-fdlibm-family.md ported the whole
 * fdlibm family into {@code types/src/fdlibm.rs} and split the registrations so
 * {@code StrictMath} gets the ported bodies while {@code Math} keeps libm. Its
 * verification was 24 {@code cargo test} unit tests over the ported ROUTINES
 * plus two probes under {@code probes/}, and {@code probes/} is never run by
 * {@code regression-suite/run.sh} at any {@code SUITE=} value. So nothing
 * scheduled ever exercised the REGISTERED NATIVES end to end, in either mode,
 * against a real JVM oracle. That is the gap this file closes: it is the only
 * StrictMath coverage in {@code regression-suite/}.
 *
 * <h2>Both modes, and it is a shared vector</h2>
 *
 * In JDK 25 {@code StrictMath.sin} and friends are NOT {@code native} — they
 * are Java methods over {@code java.lang.FdLibm}, so HotSpot's answer IS the
 * fdlibm answer by construction and is a sound oracle. CratonVM registers
 * natives over them in both shipping modes (the registrations are made from
 * {@code register_essential_natives_with_shims}, the real-JDK arm, and again
 * from {@code register_synthetic_overrides}), and those natives are
 * {@code NativeKind::Intrinsic}, which does not yield to real bytecode — so
 * these calls reach the Rust port in {@code --real-jdk} and {@code --jdk-only}
 * alike. One expectation, three arms, no mode divergence to encode.
 *
 * <h2>What the tables are, and why they can fail</h2>
 *
 * Every vector below was MEASURED on Microsoft OpenJDK 25.0.3.9 and is written
 * as raw bit patterns on both sides — Java has no hex float literal, and a
 * decimal transcription is one more place for a last-ULP mistake to enter.
 * Inputs are branch boundaries crossed with a deterministic spread. The
 * boundaries are the half that matters: every fdlibm routine is a decision tree
 * over the high word ({@code |x| < 2^-27}, {@code |x| >= 0.5}, {@code 7/16},
 * {@code |x| > 22}, {@code hx <= 0xbfd2bec3}), and a port that takes one wrong
 * branch is correct almost everywhere and wrong on a set a uniform sample never
 * visits.
 *
 * <p>Some rows are known-good discriminators rather than hopeful ones, because
 * the pre-fix body ({@code a - (a/b).round() * b}) was re-implemented and
 * replayed against this table, and these are the rows it fails:
 * {@code IEEEremainder(2.5, 1.0)} is {@code +0.5} and not {@code -0.5}
 * ({@code round} is ties-AWAY where IEEE 754 requires ties-to-EVEN), and
 * {@code IEEEremainder(MAX_VALUE, MIN_NORMAL)} is {@code +0.0} and not
 * {@code -Infinity} (the quotient overflows, while fdlibm never forms it).
 *
 * <p><b>Not {@code (1.5, 1.0)}</b>, which is the example
 * W7-54-strictmath-fdlibm-family.md §4 gives and which does NOT discriminate:
 * {@code 1.5} rounds to {@code 2} under ties-to-even and ties-away alike, so
 * both implementations answer {@code -0.5}. The half-integer quotients that
 * separate them are the ones whose ties-to-even target is the LOWER even
 * integer — {@code 2.5 -> 2}, {@code 0.5 -> 0}, {@code 4.5 -> 4} — and the
 * record happened to pick the one that agrees. All four are in the table, and
 * {@code 1.5} is kept deliberately as the negative control.
 *
 * <p>Honest limit, stated rather than implied: the sampled half can only catch
 * a function whose deviation rate is not tiny. Against the MSVC CRT the
 * measured rates ran from {@code cbrt} 30.98% down to {@code atan} 0.009%, so
 * these tables are near-certain to catch {@code cbrt}/{@code cosh}/{@code sinh}
 * and will NOT catch {@code atan} by sampling — {@code atan}'s coverage here is
 * its four branch boundaries ({@code 7/16, 11/16, 19/16, 39/16}) and nothing
 * else. A near-zero deviation rate is what makes a vacuous green easy to miss.
 *
 * <h2>Determinism</h2>
 *
 * No wall clock, no hashes, no paths, no locale. Nothing computed is printed:
 * every {@code CK} line carries a fixed count, because {@code run.sh} diffs the
 * two runs' {@code CK} lines in one session and a printed double would make the
 * transcript itself the thing under test.
 *
 * <p>NaN results are asserted as {@link Double#isNaN}, not as raw bits. Which
 * NaN a routine produces is not specified by the JLS — {@code 0.0/0.0} on x86
 * yields a QNaN with the sign bit SET, which is what the oracle recorded — and
 * freezing an unspecified bit pattern would lock a divergence in rather than
 * detect one. Signed ZERO is a different case and IS specified
 * ({@code sin(-0.0) == -0.0}), so it stays under the raw-bit comparison.
 */
public class RJdkStrictMath {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    // --- function codes; a switch rather than a method reference so this
    //     vector does not drag invokedynamic/lambda linkage into a numeric
    //     assertion. A lambda failure here would read as a StrictMath failure.
    private static final int SIN_F = 0, COS_F = 1, TAN_F = 2, ASIN_F = 3, ACOS_F = 4;
    private static final int ATAN_F = 5, EXP_F = 6, LOG_F = 7, LOG10_F = 8, CBRT_F = 9;
    private static final int LOG1P_F = 10, EXPM1_F = 11, SINH_F = 12, COSH_F = 13;
    private static final int TANH_F = 14, SQRT_F = 15;
    private static final int ATAN2_F = 16, POW_F = 17, HYPOT_F = 18, REM_F = 19;

    static double un(int f, double x) {
        switch (f) {
            case SIN_F: return StrictMath.sin(x);
            case COS_F: return StrictMath.cos(x);
            case TAN_F: return StrictMath.tan(x);
            case ASIN_F: return StrictMath.asin(x);
            case ACOS_F: return StrictMath.acos(x);
            case ATAN_F: return StrictMath.atan(x);
            case EXP_F: return StrictMath.exp(x);
            case LOG_F: return StrictMath.log(x);
            case LOG10_F: return StrictMath.log10(x);
            case CBRT_F: return StrictMath.cbrt(x);
            case LOG1P_F: return StrictMath.log1p(x);
            case EXPM1_F: return StrictMath.expm1(x);
            case SINH_F: return StrictMath.sinh(x);
            case COSH_F: return StrictMath.cosh(x);
            case TANH_F: return StrictMath.tanh(x);
            case SQRT_F: return StrictMath.sqrt(x);
            default: throw new IllegalStateException("unary code " + f);
        }
    }

    static double bin(int f, double a, double b) {
        switch (f) {
            case ATAN2_F: return StrictMath.atan2(a, b);
            case POW_F: return StrictMath.pow(a, b);
            case HYPOT_F: return StrictMath.hypot(a, b);
            case REM_F: return StrictMath.IEEEremainder(a, b);
            default: throw new IllegalStateException("binary code " + f);
        }
    }

    /**
     * One vector. Raw bits unless the oracle recorded a NaN, in which case
     * NaN-ness is the whole specified contract.
     */
    static void bits(String name, long argA, long argB, long want, double got) {
        long is = Double.doubleToRawLongBits(got);
        boolean ok = Double.isNaN(Double.longBitsToDouble(want)) ? Double.isNaN(got) : is == want;
        checks++;
        if (!ok) {
            throw new AssertionError("StrictMath." + name + " is not the fdlibm result:"
                    + " arg=0x" + Long.toHexString(argA)
                    + (argB == 0L && want == 0L ? "" : ",0x" + Long.toHexString(argB))
                    + " expected=0x" + Long.toHexString(want)
                    + " got=0x" + Long.toHexString(is));
        }
    }

    /** Replays a {argBits, resultBits} table. */
    static void unary(String name, int f, long[] t, int minRows) {
        check(t.length % 2 == 0, name + " table is not a whole number of pairs");
        check(t.length / 2 >= minRows,
                name + " table shrank to " + (t.length / 2) + " rows, below " + minRows);
        for (int i = 0; i < t.length; i += 2) {
            bits(name, t[i], 0L, t[i + 1], un(f, Double.longBitsToDouble(t[i])));
        }
    }

    /** Replays an {aBits, bBits, resultBits} table. */
    static void binary(String name, int f, long[] t, int minRows) {
        check(t.length % 3 == 0, name + " table is not a whole number of triples");
        check(t.length / 3 >= minRows,
                name + " table shrank to " + (t.length / 3) + " rows, below " + minRows);
        for (int i = 0; i < t.length; i += 3) {
            bits(name, t[i], t[i + 1], t[i + 2],
                    bin(f, Double.longBitsToDouble(t[i]), Double.longBitsToDouble(t[i + 1])));
        }
    }

    // ------------------------------------------------------------------
    // Golden tables. Measured on Microsoft OpenJDK 25.0.3.9, Windows x86-64.
    // Each row is {argument, StrictMath result} as raw bits, or
    // {a, b, result} for the binary family. Do NOT regenerate these against
    // anything but a real JVM: the point of the table is that it disagrees
    // with a platform-libm implementation.
    // ------------------------------------------------------------------

    private static final long[] SIN = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3feaed548f090ceeL,
        0xbff0000000000000L, 0xbfeaed548f090ceeL,
        0x3fe921fb54442d18L, 0x3fe6a09e667f3bccL,
        0xbfe921fb54442d18L, 0xbfe6a09e667f3bccL,
        0x3ff921fb54442d18L, 0x3ff0000000000000L,
        0x400921fb54442d18L, 0x3ca1a62633145c07L,
        0x401921fb54442d18L, 0xbcb1a62633145c07L,
        0x4002d97c7f3321d2L, 0x3fe6a09e667f3bcdL,
        0x3e40000000000000L, 0x3e40000000000000L,
        0xbe40000000000000L, 0xbe40000000000000L,
        0x3e30000000000000L, 0x3e30000000000000L,
        0x3fe0000000000000L, 0x3fdeaee8744b05f0L,
        0xbfe0000000000000L, 0xbfdeaee8744b05f0L,
        0x412e848000000000L, 0xbfd6664b2568d867L,
        0xc12e848000000000L, 0x3fd6664b2568d867L,
        0x430c6bf526340000L, 0x3feb76f88136cebaL,
        0x40c81cd6e631f8a1L, 0xbfe68298a1cec146L,
        0x7e37e43c8800759cL, 0xbfea2c16b010e385L,
        0x4120000000000000L, 0x3fc57481ec90fde3L,
        0x4130000000000000L, 0x3fd526ccb2fc8656L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0xfff8000000000000L,
        0xfff0000000000000L, 0xfff8000000000000L,
        0xbff1d87623e73820L, 0xbfecbcefa7052643L,
        0xc026b03b7e1fdd3cL, 0x3fee132ef9e3c182L,
        0x402e7aa2ea9e0de0L, 0x3fdce54750135686L,
        0x4011a46d811b7c7cL, 0xbfee8db5add1abb8L,
        0x402e4df5a10eb2d8L, 0x3fe0e19529f3c54cL,
        0xc017c9d1e55d5ed4L, 0x3fd51b80aae22badL,
        0xc0183a447ad71800L, 0x3fccb7bd45a0423eL,
        0xc033f93a18a3f7a8L, 0xbfecdbcb255f66bbL,
    };

    private static final long[] COS = {
        0x0000000000000000L, 0x3ff0000000000000L,
        0x8000000000000000L, 0x3ff0000000000000L,
        0x3ff0000000000000L, 0x3fe14a280fb5068cL,
        0xbff0000000000000L, 0x3fe14a280fb5068cL,
        0x3fe921fb54442d18L, 0x3fe6a09e667f3bcdL,
        0xbfe921fb54442d18L, 0x3fe6a09e667f3bcdL,
        0x3ff921fb54442d18L, 0x3c91a62633145c07L,
        0x400921fb54442d18L, 0xbff0000000000000L,
        0x401921fb54442d18L, 0x3ff0000000000000L,
        0x4002d97c7f3321d2L, 0xbfe6a09e667f3bccL,
        0x3e40000000000000L, 0x3ff0000000000000L,
        0xbe40000000000000L, 0x3ff0000000000000L,
        0x3e30000000000000L, 0x3ff0000000000000L,
        0x3fe0000000000000L, 0x3fec1528065b7d50L,
        0xbfe0000000000000L, 0x3fec1528065b7d50L,
        0x412e848000000000L, 0x3fedf9df9906d32cL,
        0xc12e848000000000L, 0x3fedf9df9906d32cL,
        0x430c6bf526340000L, 0xbfe06c154609d33eL,
        0x40c81cd6e631f8a1L, 0x3fe6be7c89fe4a8eL,
        0x7e37e43c8800759cL, 0xbfe2699022adc4c1L,
        0x4120000000000000L, 0x3fef8c1986ca67faL,
        0x4130000000000000L, 0x3fee33ada92fe2aeL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0xfff8000000000000L,
        0xfff0000000000000L, 0xfff8000000000000L,
        0xbff1d87623e73820L, 0x3fdc26c311d67cb4L,
        0xc026b03b7e1fdd3cL, 0x3fd5dcf0ec132433L,
        0x402e7aa2ea9e0de0L, 0xbfec8d827903ecbeL,
        0x4011a46d811b7c7cL, 0xbfd306343604359bL,
        0x402e4df5a10eb2d8L, 0xbfeb2f5deb1cd8b7L,
        0xc017c9d1e55d5ed4L, 0x3fee35a76b46fb64L,
        0xc0183a447ad71800L, 0x3fef2f29373e206dL,
        0xc033f93a18a3f7a8L, 0x3fdba75ec5c19f4aL,
    };

    private static final long[] TAN = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ff8eb245cbee3a6L,
        0xbff0000000000000L, 0xbff8eb245cbee3a6L,
        0x3fe921fb54442d18L, 0x3fefffffffffffffL,
        0xbfe921fb54442d18L, 0xbfefffffffffffffL,
        0x3ff921fb54442d18L, 0x434d02967c31cdb5L,
        0x400921fb54442d18L, 0xbca1a62633145c07L,
        0x401921fb54442d18L, 0xbcb1a62633145c07L,
        0x4002d97c7f3321d2L, 0xbff0000000000001L,
        0x3e40000000000000L, 0x3e40000000000000L,
        0xbe40000000000000L, 0xbe40000000000000L,
        0x3e30000000000000L, 0x3e30000000000000L,
        0x3fe0000000000000L, 0x3fe17b4f5bf3474aL,
        0xbfe0000000000000L, 0xbfe17b4f5bf3474aL,
        0x412e848000000000L, 0xbfd7e9768ab734c0L,
        0xc12e848000000000L, 0x3fd7e9768ab734c0L,
        0x430c6bf526340000L, 0xbffac23600a95be4L,
        0x40c81cd6e631f8a1L, 0xbfefabbca285aaa3L,
        0x7e37e43c8800759cL, 0x3ff6be411f37ac77L,
        0x4120000000000000L, 0x3fc5c354a31a846eL,
        0x4130000000000000L, 0x3fd6692e5779206fL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0xfff8000000000000L,
        0xfff0000000000000L, 0xfff8000000000000L,
        0xbff1d87623e73820L, 0xc000555a2ccee6e6L,
        0xc026b03b7e1fdd3cL, 0x4006027b036e134dL,
        0x402e7aa2ea9e0de0L, 0xbfe0312ec59c8acaL,
        0x4011a46d811b7c7cL, 0x4009b24ff73653abL,
        0x402e4df5a10eb2d8L, 0xbfe3df03e6082d87L,
        0xc017c9d1e55d5ed4L, 0x3fd65bbfb7068d15L,
        0xc0183a447ad71800L, 0x3fcd780f75e6ee66L,
        0xc033f93a18a3f7a8L, 0xc000b272c778d1a9L,
    };

    private static final long[] ASIN = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ff921fb54442d18L,
        0xbff0000000000000L, 0xbff921fb54442d18L,
        0x3fe0000000000000L, 0x3fe0c152382d7366L,
        0xbfe0000000000000L, 0xbfe0c152382d7366L,
        0x3fdffffffffffffeL, 0x3fe0c152382d7364L,
        0x3fe0000000000001L, 0x3fe0c152382d7367L,
        0x3fefffffffffffffL, 0x3ff921fb50442d18L,
        0xbfefffffffffffffL, 0xbff921fb50442d18L,
        0x3e40000000000000L, 0x3e40000000000000L,
        0x3e30000000000000L, 0x3e30000000000000L,
        0x3ff8000000000000L, 0xfff8000000000000L,
        0xbff8000000000000L, 0xfff8000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x3fe8f55ddf44fd8eL, 0x3feca07950d4f1f5L,
        0xbfecbce5439f8d6aL, 0xbff1d86a54f96fb5L,
        0xbfd62f73d796a324L, 0xbfd6a7d38fe58fcdL,
        0xbfd0f566b49270d0L, 0xbfd129e1862eaa53L,
        0xbfd31012dd542ab4L, 0xbfd35b4668df0740L,
        0xbfdcad34dcf1df38L, 0xbfddbc1fe490f7e6L,
        0xbfb67fbb4e9c9030L, 0xbfb6872c1b830353L,
        0x3fe8633c26c63a54L, 0x3febbaf418a05aa8L,
    };

    private static final long[] ACOS = {
        0x0000000000000000L, 0x3ff921fb54442d18L,
        0x8000000000000000L, 0x3ff921fb54442d18L,
        0x3ff0000000000000L, 0x0000000000000000L,
        0xbff0000000000000L, 0x400921fb54442d18L,
        0x3fe0000000000000L, 0x3ff0c152382d7366L,
        0xbfe0000000000000L, 0x4000c152382d7366L,
        0x3fdffffffffffffeL, 0x3ff0c152382d7366L,
        0x3fe0000000000001L, 0x3ff0c152382d7365L,
        0x3fefffffffffffffL, 0x3e50000000000000L,
        0xbfefffffffffffffL, 0x400921fb52442d18L,
        0x3e40000000000000L, 0x3ff921fb52442d18L,
        0x3e30000000000000L, 0x3ff921fb53442d18L,
        0x3ff8000000000000L, 0xfff8000000000000L,
        0xbff8000000000000L, 0xfff8000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x3fe8f55ddf44fd8eL, 0x3fe5a37d57b3683cL,
        0xbfecbce5439f8d6aL, 0x40057d32d49ece67L,
        0xbfd62f73d796a324L, 0x3ffecbf0383d910cL,
        0xbfd0f566b49270d0L, 0x3ffd6c73b5cfd7adL,
        0xbfd31012dd542ab4L, 0x3ffdf8ccee7beee8L,
        0xbfdcad34dcf1df38L, 0x40004881a6b43589L,
        0xbfb67fbb4e9c9030L, 0x3ffa8a6e15fc5d4dL,
        0x3fe8633c26c63a54L, 0x3fe689028fe7ff89L,
    };

    private static final long[] ATAN = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3fe921fb54442d18L,
        0xbff0000000000000L, 0xbfe921fb54442d18L,
        0x3fdc000000000000L, 0x3fda64eec3cc23fdL,
        0x3fe6000000000000L, 0x3fe345f01cce37bbL,
        0x3ff3000000000000L, 0x3febde70ed439fe7L,
        0x4003800000000000L, 0x3ff2e75728833a54L,
        0xbfdc000000000000L, 0xbfda64eec3cc23fdL,
        0xbfe6000000000000L, 0xbfe345f01cce37bbL,
        0xbff3000000000000L, 0xbfebde70ed439fe7L,
        0xc003800000000000L, 0xbff2e75728833a54L,
        0x3e20000000000000L, 0x3e20000000000000L,
        0xbe20000000000000L, 0xbe20000000000000L,
        0x4410000000000000L, 0x3ff921fb54442d18L,
        0xc410000000000000L, 0xbff921fb54442d18L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x3ff921fb54442d18L,
        0xfff0000000000000L, 0xbff921fb54442d18L,
        0x403a489e5b2a43ccL, 0x3ff88637f9bb1731L,
        0x402d82940a117ee8L, 0x3ff80cce338740c6L,
        0x403fcb1ce0f9c6a8L, 0x3ff8a13146786f9fL,
        0x4029078793eb7540L, 0x3ff7db6178d0a398L,
        0x40462501028c8424L, 0x3ff8c583a29b7caaL,
        0x4033b323f62dc964L, 0x3ff8523d241ef3afL,
        0xc020ec6339701cfcL, 0xbff740271e2ed517L,
        0xc03c06c922ed0d7bL, 0xbff88fe578343b17L,
    };

    private static final long[] EXP = {
        0x0000000000000000L, 0x3ff0000000000000L,
        0x8000000000000000L, 0x3ff0000000000000L,
        0x3ff0000000000000L, 0x4005bf0a8b14576aL,
        0xbff0000000000000L, 0x3fd78b56362cef38L,
        0x3fe62e42fefa39efL, 0x4000000000000000L,
        0x3fd62e42fefa39efL, 0x3ff6a09e667f3bccL,
        0x3ff0a2b23f3bab73L, 0x4006a09e667f3bccL,
        0xbfd62e42fefa39efL, 0x3fe6a09e667f3bccL,
        0xbff0a2b23f3bab73L, 0x3fd6a09e667f3bcdL,
        0x40862e42fefa39efL, 0x7fefffffffffff2aL,
        0x40862e42fefa39f0L, 0x7ff0000000000000L,
        0xc0874910d52d3051L, 0x0000000000000001L,
        0xc0874910d52d3052L, 0x0000000000000000L,
        0x3e30000000000000L, 0x3ff0000001000000L,
        0xbe30000000000000L, 0x3feffffffe000000L,
        0x4056000000000000L, 0x47df1056dc7bf22dL,
        0xc056000000000000L, 0x38007b7112bc1ffeL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0x0000000000000000L,
        0xc049d6f90b235c30L, 0x3b45bd6e0b39ab15L,
        0x407df2d09a5ed674L, 0x6b23c3590c27b236L,
        0x4060926e0469a80cL, 0x4be33f7cd1fbf0ceL,
        0xc05bffca708ad548L, 0x35d57312041e9224L,
        0xc065c24f11c5c544L, 0x303d2e660b7dd281L,
        0xc06f084d669f32e0L, 0x298c9676945ebc16L,
        0xc07dd4c6ca514733L, 0x14e52adc386ea522L,
        0xc04c0257793d01f0L, 0x3ae228ef3489d848L,
    };

    private static final long[] LOG = {
        0x3ff0000000000000L, 0x0000000000000000L,
        0x4000000000000000L, 0x3fe62e42fefa39efL,
        0x4024000000000000L, 0x40026bb1bbb55516L,
        0x3fe0000000000000L, 0xbfe62e42fefa39efL,
        0x4005bf0a8b145769L, 0x3ff0000000000000L,
        0x3fe6a09e667f3bcdL, 0xbfd62e42fefa39eeL,
        0x3fe6a09e667f3bcdL, 0xbfd62e42fefa39eeL,
        0x0000000000000001L, 0xc0874385446d71c3L,
        0x0010000000000000L, 0xc086232bdd7abcd2L,
        0x7fefffffffffffffL, 0x40862e42fefa39efL,
        0x0000000000000000L, 0xfff0000000000000L,
        0x8000000000000000L, 0xfff0000000000000L,
        0xbff0000000000000L, 0xfff8000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0x3ff0000000000001L, 0x3cafffffffffffffL,
        0x3fefffffffffffffL, 0xbca0000000000000L,
        0x3fe3f41a22ee626cL, 0xbfde3aa835394df0L,
        0x3fea3cc25bdcf659L, 0xbfc96a2953c7c825L,
        0x3fe6eb53757a1adfL, 0xbfd55c4f03c2cc9dL,
        0x3fb21ea0398123c4L, 0xc0052f79bf7f4f8eL,
        0x3feec4f65070b019L, 0xbfa414163e8a1194L,
        0x3fd6116a0eb84c3fL, 0xbff1093ad5c3c51dL,
        0x3fe40d59b56aaa2eL, 0xbfdde9dfab1d96cdL,
        0x3fe110e94ebb12f9L, 0xbfe41dda74fe8577L,
    };

    private static final long[] LOG10 = {
        0x3ff0000000000000L, 0x0000000000000000L,
        0x4024000000000000L, 0x3ff0000000000000L,
        0x4059000000000000L, 0x4000000000000000L,
        0x408f400000000000L, 0x4008000000000000L,
        0x3ddb7cdfd9d7bdbbL, 0xc024000000000000L,
        0x3fe0000000000000L, 0xbfd34413509f79ffL,
        0x4000000000000000L, 0x3fd34413509f79ffL,
        0x4005bf0a8b145769L, 0x3fdbcb7b1526e50eL,
        0x0000000000000001L, 0xc07434e6420f4374L,
        0x0010000000000000L, 0xc0733a7146f72a42L,
        0x7fefffffffffffffL, 0x40734413509f79ffL,
        0x0000000000000000L, 0xfff0000000000000L,
        0x8000000000000000L, 0xfff0000000000000L,
        0xbff0000000000000L, 0xfff8000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0x4127ce2142b47b89L, 0x40179188586677c4L,
        0x4104972f4a6aca90L, 0x4014e881f2d9b694L,
        0x41271e170885f137L, 0x4017847f36cd9178L,
        0x41042174476ec22cL, 0x4014de765d599893L,
        0x4121c4ef7875145aL, 0x40170f7b8cb72145L,
        0x4128f438924745f4L, 0x4017a67df785d888L,
        0x4121625e0edbb231L, 0x401705bd8954a584L,
        0x41285d367bbdac0eL, 0x40179bda74dd237eL,
    };

    private static final long[] CBRT = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ff0000000000000L,
        0xbff0000000000000L, 0xbff0000000000000L,
        0x4020000000000000L, 0x4000000000000000L,
        0xc020000000000000L, 0xc000000000000000L,
        0x403b000000000000L, 0x4008000000000000L,
        0xc03b000000000000L, 0xc008000000000000L,
        0x4050000000000000L, 0x4010000000000000L,
        0x3fc0000000000000L, 0x3fe0000000000000L,
        0x0000000000000001L, 0x2990000000000000L,
        0x8000000000000001L, 0xa990000000000000L,
        0x0010000000000000L, 0x2aa428a2f98d728bL,
        0x7fefffffffffffffL, 0x554428a2f98d728bL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0xfff0000000000000L,
        0xc07933a3eedb429aL, 0xc01d8d228bad3b00L,
        0xc01e0a00acd50e80L, 0xbfff5520d2f7b45dL,
        0x4081876367e826d8L, 0x40207e83cb4262e2L,
        0xc06fea8d990c38d0L, 0xc0196050cc3193a5L,
        0xc08ab74d53053de3L, 0xc022fb56de064ec9L,
        0x4063a97957ecfda8L, 0x401597a22d310241L,
        0x408969b5d486543cL, 0x4022ab034ed8d72fL,
        0xc089747ee65c6975L, 0xc022ada7045f1e7dL,
    };

    private static final long[] LOG1P = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3fe62e42fefa39efL,
        0xbff0000000000000L, 0xfff0000000000000L,
        0xbfd2bec333018867L, 0xbfd62e42fefa39efL,
        0xbfd2bec333018866L, 0xbfd62e42fefa39eeL,
        0xbfd2bec333018868L, 0xbfd62e42fefa39f1L,
        0x3fda827999fcef33L, 0x3fd62e42fefa39f0L,
        0x3c90000000000000L, 0x3c90000000000000L,
        0xbc90000000000000L, 0xbc90000000000000L,
        0x3e20000000000000L, 0x3e1fffffff800000L,
        0xbfe0000000000000L, 0xbfe62e42fefa39efL,
        0x4000000000000000L, 0x3ff193ea7aad030aL,
        0x4341c37937e08000L, 0x40426bb1bbb55516L,
        0xbfefffffffffffffL, 0xc0425e4f7b2737faL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0x40020189c28fa85cL, 0x3ff2dcb7a096aea6L,
        0x3fe9e3599afd7badL, 0x3fe2f804ba95e2d0L,
        0x3fea893c034b687bL, 0x3fe353355a36ea37L,
        0x4000485d9b45c2c2L, 0x3ff1c3e0b76d8f51L,
        0x3fcc04ac3df56114L, 0x3fc95630166d2150L,
        0xbfec29f50bdfcb30L, 0xc000f860d86eb772L,
        0x401151f8c11cbb05L, 0x3ffac615e13d59feL,
        0x3ffd20a9755d8506L, 0x3ff09729b0f853c7L,
    };

    private static final long[] EXPM1 = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ffb7e151628aed2L,
        0xbff0000000000000L, 0xbfe43a54e4e98864L,
        0x3fd62e42fefa39efL, 0x3fda827999fcef32L,
        0x3ff0a2b23f3bab73L, 0x3ffd413cccfe7798L,
        0xbfd62e42fefa39efL, 0xbfd2bec333018867L,
        0xbff0a2b23f3bab73L, 0xbfe4afb0ccc0621aL,
        0x4043687a9f1af2b1L, 0x436fffffffffffecL,
        0xbfd0000000000000L, 0xbfcc5041854df7d4L,
        0x3c90000000000000L, 0x3c90000000000000L,
        0xbc90000000000000L, 0xbc90000000000000L,
        0x40862e42fefa39f0L, 0x7ff0000000000000L,
        0xc04c000000000000L, 0xbff0000000000000L,
        0x4043687a9f1af2b1L, 0x436fffffffffffecL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0xbff0000000000000L,
        0xc03099fdcff80213L, 0xbfefffffdee4a7daL,
        0x40388c4a29011de0L, 0x42255647823eaec8L,
        0xc002b575edf0a578L, 0xbfece9c80f671190L,
        0x401d8dfa304b8608L, 0x40994215009eed1eL,
        0x400f67d7e49251e0L, 0x4048d82a389563feL,
        0xc02a972178c485b2L, 0xbfeffffc78aad026L,
        0x400732af52695210L, 0x40312b693b7056ecL,
        0x4024426ad928cab0L, 0x40d87d182e9f9f02L,
    };

    private static final long[] SINH = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ff2cd9fc44eb982L,
        0xbff0000000000000L, 0xbff2cd9fc44eb982L,
        0x4036000000000000L, 0x41dab5adb9c43600L,
        0xc036000000000000L, 0xc1dab5adb9c43600L,
        0x4036000000000001L, 0x41dab5adb9c4361aL,
        0x3fd62e42fefa39efL, 0x3fd6a09e667f3bccL,
        0xbfd62e42fefa39efL, 0xbfd6a09e667f3bccL,
        0x3e30000000000000L, 0x3e30000000000000L,
        0xbe30000000000000L, 0xbe30000000000000L,
        0x3c80000000000000L, 0x3c80000000000000L,
        0x40862e42fefa39f0L, 0x7fe0000000000196L,
        0x408633ce8fb9f87dL, 0x7feffffffffffd3bL,
        0xc08633ce8fb9f87dL, 0xffeffffffffffd3bL,
        0x408633ce00000000L, 0x7feffdc12c49e31bL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0xfff0000000000000L,
        0xc038a2d46f4b0c88L, 0xc2174d06e2da222fL,
        0x40216594b8c14470L, 0x40a76951170895deL,
        0xc01cd4642889fce4L, 0xc0851596124e82e7L,
        0xc037ff95f193d0daL, 0xc208a1496c44953eL,
        0x4017f78e8156b19cL, 0x406901d0f9bf25efL,
        0xc036e05b9475f898L, 0xc1f00a6b3b32d98aL,
        0x4035d5d57f5922dcL, 0x41d6a74cd91b296fL,
        0xc036ea0cce7cbad6L, 0xc1f0a8dee02661b0L,
    };

    private static final long[] COSH = {
        0x0000000000000000L, 0x3ff0000000000000L,
        0x8000000000000000L, 0x3ff0000000000000L,
        0x3ff0000000000000L, 0x3ff8b07551d9f551L,
        0xbff0000000000000L, 0x3ff8b07551d9f551L,
        0x4036000000000000L, 0x41dab5adb9c43600L,
        0xc036000000000000L, 0x41dab5adb9c43600L,
        0x4036000000000001L, 0x41dab5adb9c4361aL,
        0x3fd62e42fefa39efL, 0x3ff0f876ccdf6cd9L,
        0xbfd62e42fefa39efL, 0x3ff0f876ccdf6cd9L,
        0x3e30000000000000L, 0x3ff0000000000000L,
        0xbe30000000000000L, 0x3ff0000000000000L,
        0x3c80000000000000L, 0x3ff0000000000000L,
        0x40862e42fefa39f0L, 0x7fe0000000000196L,
        0x408633ce8fb9f87dL, 0x7feffffffffffd3bL,
        0xc08633ce8fb9f87dL, 0x7feffffffffffd3bL,
        0x408633ce00000000L, 0x7feffdc12c49e31bL,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0x7ff0000000000000L,
        0xc038a2d46f4b0c88L, 0x42174d06e2da222fL,
        0x40216594b8c14470L, 0x40a769512ce73a0eL,
        0xc01cd4642889fce4L, 0x4085159796d774dfL,
        0xc037ff95f193d0daL, 0x4208a1496c44953eL,
        0x4017f78e8156b19cL, 0x406901e5731b3f35L,
        0xc036e05b9475f898L, 0x41f00a6b3b32d98aL,
        0x4035d5d57f5922dcL, 0x41d6a74cd91b296fL,
        0xc036ea0cce7cbad6L, 0x41f0a8dee02661b0L,
    };

    private static final long[] TANH = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3fe85efab514f394L,
        0xbff0000000000000L, 0xbfe85efab514f394L,
        0x4036000000000000L, 0x3ff0000000000000L,
        0xc036000000000000L, 0xbff0000000000000L,
        0x4036000000000001L, 0x3ff0000000000000L,
        0x3fd62e42fefa39efL, 0x3fd5555555555555L,
        0xbfd62e42fefa39efL, 0xbfd5555555555555L,
        0x3e30000000000000L, 0x3e30000000000000L,
        0xbe30000000000000L, 0xbe30000000000000L,
        0x3c80000000000000L, 0x3c80000000000000L,
        0x40862e42fefa39f0L, 0x3ff0000000000000L,
        0x408633ce8fb9f87dL, 0x3ff0000000000000L,
        0xc08633ce8fb9f87dL, 0xbff0000000000000L,
        0x408633ce00000000L, 0x3ff0000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x3ff0000000000000L,
        0xfff0000000000000L, 0xbff0000000000000L,
        0xc038a2d46f4b0c88L, 0xbff0000000000000L,
        0x40216594b8c14470L, 0x3fefffffe21b76fdL,
        0xc01cd4642889fce4L, 0xbfeffffdb250ae82L,
        0xc037ff95f193d0daL, 0xbff0000000000000L,
        0x4017f78e8156b19cL, 0x3fefffe5cd0bf755L,
        0xc036e05b9475f898L, 0xbff0000000000000L,
        0x4035d5d57f5922dcL, 0x3ff0000000000000L,
        0xc036ea0cce7cbad6L, 0xbff0000000000000L,
    };

    // `sqrt` needs no fdlibm port and never will: IEEE 754 REQUIRES sqrt to be
    // correctly rounded, so the hardware instruction and `FdLibm.Sqrt.compute`
    // compute the same function by construction. Its zero deviation rate is a
    // theorem, not a measurement. This table exists so that a missing test and
    // a test documenting why no port is needed do not look identical from a
    // distance — it pins the values where a non-conforming implementation
    // (subnormals, MIN_VALUE, MAX_VALUE, the ULP neighbours of 1.0, signed
    // zero) would show first.
    private static final long[] SQRT = {
        0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x3ff0000000000000L,
        0x4000000000000000L, 0x3ff6a09e667f3bcdL,
        0x4010000000000000L, 0x4000000000000000L,
        0x4062000000000000L, 0x4028000000000000L,
        0x3fd0000000000000L, 0x3fe0000000000000L,
        0x7e37e43c8800759cL, 0x5f138d352e5096afL,
        0x01a56e1fc2f8f359L, 0x20ca2fe76a3f9475L,
        0x0000000000000001L, 0x1e60000000000000L,
        0x0010000000000000L, 0x2000000000000000L,
        0x7fefffffffffffffL, 0x5fefffffffffffffL,
        0xbff0000000000000L, 0xfff8000000000000L,
        0x8000000000000000L, 0x8000000000000000L,
        0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff0000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0xfff8000000000000L,
        0x3fefffffffffffffL, 0x3fefffffffffffffL,
        0x3ff0000000000001L, 0x3ff0000000000000L,
    };

    // Argument order is load-bearing and is NOT alphabetical: the JDK declares
    // `atan2(double y, double x)` — ORDINATE FIRST — so column 0 is `y`.
    // Transposing them is a defect no accuracy test can see, because atan2(y,x)
    // and atan2(x,y) are both plausible angles and only the quadrant is wrong.
    // The full sign/zero/infinity matrix at the head of this table is what
    // catches it.
    private static final long[] ATAN2 = {
        0x0000000000000000L, 0x3ff0000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x3ff0000000000000L, 0x8000000000000000L,
        0x0000000000000000L, 0xbff0000000000000L, 0x400921fb54442d18L,
        0x8000000000000000L, 0xbff0000000000000L, 0xc00921fb54442d18L,
        0x3ff0000000000000L, 0x0000000000000000L, 0x3ff921fb54442d18L,
        0xbff0000000000000L, 0x0000000000000000L, 0xbff921fb54442d18L,
        0x3ff0000000000000L, 0x8000000000000000L, 0x3ff921fb54442d18L,
        0xbff0000000000000L, 0x8000000000000000L, 0xbff921fb54442d18L,
        0x0000000000000000L, 0x0000000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x8000000000000000L, 0xc00921fb54442d18L,
        0x3ff0000000000000L, 0x3ff0000000000000L, 0x3fe921fb54442d18L,
        0xbff0000000000000L, 0x3ff0000000000000L, 0xbfe921fb54442d18L,
        0x3ff0000000000000L, 0xbff0000000000000L, 0x4002d97c7f3321d2L,
        0xbff0000000000000L, 0xbff0000000000000L, 0xc002d97c7f3321d2L,
        0x7ff0000000000000L, 0x7ff0000000000000L, 0x3fe921fb54442d18L,
        0x7ff0000000000000L, 0xfff0000000000000L, 0x4002d97c7f3321d2L,
        0xfff0000000000000L, 0x7ff0000000000000L, 0xbfe921fb54442d18L,
        0xfff0000000000000L, 0xfff0000000000000L, 0xc002d97c7f3321d2L,
        0x3ff0000000000000L, 0x7ff0000000000000L, 0x0000000000000000L,
        0x7ff0000000000000L, 0x3ff0000000000000L, 0x3ff921fb54442d18L,
        0x7ff8000000000000L, 0x3ff0000000000000L, 0x7ff8000000000000L,
        0x3ff0000000000000L, 0x7ff8000000000000L, 0x7ff8000000000000L,
        0x01a56e1fc2f8f359L, 0x7e37e43c8800759cL, 0x0000000000000000L,
        0x7e37e43c8800759cL, 0x01a56e1fc2f8f359L, 0x3ff921fb54442d18L,
        0x4019a57dbad1304cL, 0xc01b5cc3430574dfL, 0x40031bc1df2b2b78L,
        0xc017c0c0817e6ddeL, 0xc01e7f1adfce976eL, 0xc003d6c46052b411L,
        0x4021f56823087b38L, 0xbff54309d0e73f88L, 0x3ffb7bca06458530L,
        0x4007503820dab200L, 0x4007bc4cf0016c50L, 0x3fe8d87913e91b67L,
        0xc01695328893262eL, 0xc011bbaf03fdae9cL, 0xc001e44eb707d070L,
        0xc00e9cac26d9e9eaL, 0x401665e065f5b03aL, 0xbfe32ee5aa0502b9L,
        0xc0217af8a57c70deL, 0x3fce38f191cd4a80L, 0xbff8b35b4ffc4451L,
        0x4022ca5e30c4ee08L, 0x402155119686838cL, 0x3fea6c7eed7e1c9cL,
    };

    private static final long[] POW = {
        0x0000000000000000L, 0x0000000000000000L, 0x3ff0000000000000L,
        0x3ff0000000000000L, 0x7ff8000000000000L, 0x7ff8000000000000L,
        0x7ff8000000000000L, 0x0000000000000000L, 0x3ff0000000000000L,
        0x4000000000000000L, 0x0000000000000000L, 0x3ff0000000000000L,
        0x4000000000000000L, 0x8000000000000000L, 0x3ff0000000000000L,
        0xbff0000000000000L, 0x7ff0000000000000L, 0xfff8000000000000L,
        0xbff0000000000000L, 0xfff0000000000000L, 0xfff8000000000000L,
        0x3fe0000000000000L, 0x7ff0000000000000L, 0x0000000000000000L,
        0x4000000000000000L, 0x7ff0000000000000L, 0x7ff0000000000000L,
        0x0000000000000000L, 0xbff0000000000000L, 0x7ff0000000000000L,
        0x8000000000000000L, 0xbff0000000000000L, 0xfff0000000000000L,
        0x8000000000000000L, 0xc000000000000000L, 0x7ff0000000000000L,
        0x8000000000000000L, 0x4008000000000000L, 0x8000000000000000L,
        0x8000000000000000L, 0x4000000000000000L, 0x0000000000000000L,
        0xfff0000000000000L, 0x4008000000000000L, 0xfff0000000000000L,
        0xfff0000000000000L, 0x4000000000000000L, 0x7ff0000000000000L,
        0xfff0000000000000L, 0xc008000000000000L, 0x8000000000000000L,
        0xc000000000000000L, 0x4008000000000000L, 0xc020000000000000L,
        0xc000000000000000L, 0x4000000000000000L, 0x4010000000000000L,
        0xc000000000000000L, 0x4004000000000000L, 0xfff8000000000000L,
        0x4000000000000000L, 0x3fe0000000000000L, 0x3ff6a09e667f3bcdL,
        0x4000000000000000L, 0xbfe0000000000000L, 0x3fe6a09e667f3bccL,
        0x4024000000000000L, 0x4073400000000000L, 0x7fe1ccf385ebc8a0L,
        0x4024000000000000L, 0xc073400000000000L, 0x000730d67819e8d2L,
        0x3ff0000000000001L, 0x430c6bf526340000L, 0x3ff3fa60615291eeL,
        0x3fefffffffffffffL, 0x430c6bf526340000L, 0x3feca32cbada6c6aL,
        0x7fefffffffffffffL, 0x4000000000000000L, 0x7ff0000000000000L,
        0x0000000000000001L, 0x3fe0000000000000L, 0x1e60000000000000L,
        0x4008000000000000L, 0x404f800000000000L, 0x462ce48dca5fa622L,
        0x4008000000000000L, 0xc04f800000000000L, 0x39b1b87ecd2fb22fL,
        0x3ff8000000000000L, 0x404f000000000000L, 0x423343093195196cL,
        0x4055151eeccdcdbfL, 0x4036ad5130b56c1aL, 0x4900fe93bc3b95d3L,
        0x3fe5d04eb33bfbbeL, 0x402d30b3a9b5681cL, 0x3f6e82e51c10dcebL,
        0x40304cb7f59f0fecL, 0xc0258e95652e843aL, 0x3d3835a5fcfae88bL,
        0x403e1788117fec51L, 0x401eb0745d436810L, 0x4249a755deb5c12cL,
        0x4055b4ee74f1795bL, 0xc01e42e16a448f88L, 0x3ce36952f953c0f5L,
        0x403cf7728b9930d7L, 0x403dc53a24821f7aL, 0x48f7d41c7c5927f3L,
        0x40333419e18bf9deL, 0x40373a6bee802f92L, 0x46205295da8f6e55L,
        0x4031a837a7096e92L, 0xc039f80edc0188b9L, 0x3935959a7b35385eL,
    };

    private static final long[] HYPOT = {
        0x4008000000000000L, 0x4010000000000000L, 0x4014000000000000L,
        0x0000000000000000L, 0x0000000000000000L, 0x0000000000000000L,
        0xc008000000000000L, 0xc010000000000000L, 0x4014000000000000L,
        0x3ff0000000000000L, 0x0000000000000000L, 0x3ff0000000000000L,
        0x0000000000000000L, 0x3ff0000000000000L, 0x3ff0000000000000L,
        0x7fefffffffffffffL, 0x7fefffffffffffffL, 0x7ff0000000000000L,
        0x0000000000000001L, 0x0000000000000001L, 0x0000000000000001L,
        0x7e37e43c8800759cL, 0x01a56e1fc2f8f359L, 0x7e37e43c8800759cL,
        0x01a56e1fc2f8f359L, 0x7e37e43c8800759cL, 0x7e37e43c8800759cL,
        0x7ff0000000000000L, 0x7ff8000000000000L, 0x7ff0000000000000L,
        0x7ff8000000000000L, 0x7ff0000000000000L, 0x7ff0000000000000L,
        0x7ff8000000000000L, 0x3ff0000000000000L, 0x7ff8000000000000L,
        0x3ff0000000000000L, 0x43b0000000000000L, 0x43b0000000000000L,
        0x43b0000000000000L, 0x3ff0000000000000L, 0x43b0000000000000L,
        0x4202a05f20000000L, 0x3ddb7cdfd9d7bdbbL, 0x4202a05f20000000L,
        0x40dc467069dc1b28L, 0x40e28124029b8f20L, 0x40e7497b0dc51a87L,
        0xc0d8fab4118c9d28L, 0xc0e4e02702989e60L, 0x40e8539a0714c46dL,
        0xc0e45ba9d5c59a8dL, 0x40ee4bd8382299e8L, 0x40f2401cf0624cdcL,
        0x40e49cc375003fbcL, 0x40f13bc7040b87e4L, 0x40f4148270071ffcL,
        0x40da495853a44fdcL, 0x40e012faef1a3e60L, 0x40e4c374fecd8aeaL,
        0x40ee7d5504ffe31cL, 0x40e11ef36f230ed8L, 0x40f17bdd6d0b65faL,
        0xc0e46ed29ddf08c0L, 0xc0e1f64f248fc77dL, 0x40eb349f1e31987aL,
        0xc0ebf0881ffcb44aL, 0x40b1ffe2853b80a0L, 0x40ec07af9e34b868L,
    };

    // `IEEEremainder` is the one row of this surface that was never a
    // last-ULP story: 49.83% of sampled pairs disagreed with fdlibm and the
    // worst error was UNBOUNDED. Both `Math` and `StrictMath` spell their
    // contract "as prescribed by the IEEE 754 standard", which fixes the
    // result exactly, so there is no accuracy latitude for either class to
    // hide in. Rows 1 and 10 are the two the previous
    // `a - (a/b).round() * b` body got wrong; see the named assertions in
    // `namedDiscriminators`.
    private static final long[] REM = {
        0x3ff8000000000000L, 0x3ff0000000000000L, 0xbfe0000000000000L,
        0x4004000000000000L, 0x3ff0000000000000L, 0x3fe0000000000000L,
        0x3fe0000000000000L, 0x3ff0000000000000L, 0x3fe0000000000000L,
        0xbff8000000000000L, 0x3ff0000000000000L, 0x3fe0000000000000L,
        0xc004000000000000L, 0x3ff0000000000000L, 0xbfe0000000000000L,
        0x400c000000000000L, 0x3ff0000000000000L, 0xbfe0000000000000L,
        0x4012000000000000L, 0x3ff0000000000000L, 0x3fe0000000000000L,
        0x3ff8000000000000L, 0xbff0000000000000L, 0xbfe0000000000000L,
        0xbfe0000000000000L, 0x3ff0000000000000L, 0xbfe0000000000000L,
        0x7fefffffffffffffL, 0x0010000000000000L, 0x0000000000000000L,
        0x7fefffffffffffffL, 0x0000000000000001L, 0x0000000000000000L,
        0x7fefffffffffffffL, 0x4008000000000000L, 0xbff0000000000000L,
        0x7e37e43c8800759cL, 0x01a56e1fc2f8f359L, 0x0194f722a6f79f9cL,
        0x4014000000000000L, 0x4008000000000000L, 0xbff0000000000000L,
        0xc014000000000000L, 0x4008000000000000L, 0x3ff0000000000000L,
        0x4014000000000000L, 0xc008000000000000L, 0xbff0000000000000L,
        0xc014000000000000L, 0xc008000000000000L, 0x3ff0000000000000L,
        0x0000000000000000L, 0x3ff0000000000000L, 0x0000000000000000L,
        0x8000000000000000L, 0x3ff0000000000000L, 0x8000000000000000L,
        0x3ff0000000000000L, 0x0000000000000000L, 0xfff8000000000000L,
        0x3ff0000000000000L, 0x7ff0000000000000L, 0x3ff0000000000000L,
        0x7ff0000000000000L, 0x3ff0000000000000L, 0xfff8000000000000L,
        0x7ff8000000000000L, 0x3ff0000000000000L, 0x7ff8000000000000L,
        0x3ff0000000000000L, 0x7ff8000000000000L, 0x7ff8000000000000L,
        0x401c000000000000L, 0x4000000000000000L, 0xbff0000000000000L,
        0x4022000000000000L, 0x4000000000000000L, 0x3ff0000000000000L,
        0x4018000000000000L, 0x4010000000000000L, 0xc000000000000000L,
        0x4024000000000000L, 0x4010000000000000L, 0x4000000000000000L,
        0xc08b4015dc368550L, 0x40341d82716c6ea4L, 0xc01c3749e66851d0L,
        0xc073b8e15facc376L, 0xc0248a3c10b05710L, 0x400684683f0870c0L,
        0xc0437e34fd91af30L, 0x40434447ac6b1206L, 0xbfdcf6a8934e9500L,
        0xc08354f8167d1c7eL, 0xc01dd848cb328e20L, 0x3fe51c7516aeab00L,
        0xc08bff73a4c8f2ceL, 0xc042e00cc5eebaebL, 0x402427e1074964a0L,
        0xc0832fb93757639fL, 0xc04847676f4c8048L, 0x4031495a62d89370L,
        0xc088acb507da1430L, 0xc0442db68a796a4eL, 0x40318de4a7b61630L,
        0x408e387a1c8bd8a8L, 0xc0386c005cefd232L, 0xc023a195e7fb85a0L,
    };

    // ------------------------------------------------------------------

    /** The eighteen fdlibm routines plus the sqrt theorem, replayed by bits. */
    static void fdlibmFamily() {
        unary("sin", SIN_F, SIN, 30);
        unary("cos", COS_F, COS, 30);
        unary("tan", TAN_F, TAN, 30);
        unary("asin", ASIN_F, ASIN, 20);
        unary("acos", ACOS_F, ACOS, 20);
        unary("atan", ATAN_F, ATAN, 24);
        unary("exp", EXP_F, EXP, 25);
        unary("log", LOG_F, LOG, 22);
        unary("log10", LOG10_F, LOG10, 21);
        unary("cbrt", CBRT_F, CBRT, 22);
        unary("log1p", LOG1P_F, LOG1P, 22);
        unary("expm1", EXPM1_F, EXPM1, 23);
        unary("sinh", SINH_F, SINH, 24);
        unary("cosh", COSH_F, COSH, 24);
        unary("tanh", TANH_F, TANH, 24);
        unary("sqrt", SQRT_F, SQRT, 16);
        binary("atan2", ATAN2_F, ATAN2, 29);
        binary("pow", POW_F, POW, 36);
        binary("hypot", HYPOT_F, HYPOT, 20);
        binary("IEEEremainder", REM_F, REM, 33);
        System.out.println("CK RJdkStrictMath functions=20");
    }

    /**
     * The vectors that separate fdlibm from the previous
     * {@code a - (a/b).round() * b} body, asserted by name so a reader does not
     * have to decode hex to see that this table can go red. Each was checked by
     * re-implementing that body and replaying it: the ones marked DISCRIMINATOR
     * fail under it, the one marked CONTROL passes under it.
     */
    static void namedDiscriminators() {
        // `f64::round` is ties-AWAY-from-zero; IEEE 754 requires the quotient
        // rounded to nearest with ties to EVEN. That only shows on a
        // half-integer quotient whose even neighbour is the LOWER one.
        //
        // CONTROL — 1.5 rounds to 2 under BOTH rules, so both implementations
        // answer -0.5 and this row proves nothing on its own. It is asserted
        // anyway, because it is the row W7-54 §4 cites as the discriminator and
        // a later reader deleting it as redundant should see why it is here.
        check(Double.doubleToRawLongBits(StrictMath.IEEEremainder(1.5, 1.0))
                        == Double.doubleToRawLongBits(-0.5),
                "IEEEremainder(1.5, 1.0) must be -0.5");
        // DISCRIMINATOR — 2.5 rounds to 2 (even), not 3 (away).
        check(Double.doubleToRawLongBits(StrictMath.IEEEremainder(2.5, 1.0))
                        == Double.doubleToRawLongBits(0.5),
                "IEEEremainder(2.5, 1.0) must be +0.5 (ties-to-even), not -0.5 (ties-away)");
        // DISCRIMINATOR — 0.5 rounds to 0 (even), not 1 (away).
        check(Double.doubleToRawLongBits(StrictMath.IEEEremainder(0.5, 1.0))
                        == Double.doubleToRawLongBits(0.5),
                "IEEEremainder(0.5, 1.0) must be +0.5 (ties-to-even), not -0.5 (ties-away)");
        // DISCRIMINATOR — 4.5 rounds to 4 (even), not 5 (away).
        check(Double.doubleToRawLongBits(StrictMath.IEEEremainder(4.5, 1.0))
                        == Double.doubleToRawLongBits(0.5),
                "IEEEremainder(4.5, 1.0) must be +0.5 (ties-to-even), not -0.5 (ties-away)");
        // DISCRIMINATOR — forming the quotient at all overflows for operands
        // whose remainder is perfectly ordinary: MAX_VALUE / MIN_NORMAL is
        // +inf, so `a - inf*b` is -Infinity (NOT NaN, which is what W7-54 §4
        // predicts — measured). fdlibm never forms the quotient, reducing by
        // fmod against 2p and finishing with two conditional subtractions, so
        // the result is exact by construction. `isFinite`, not `!isNaN`:
        // the broken answer IS an infinity and `!isNaN` would admit it.
        double huge = StrictMath.IEEEremainder(Double.MAX_VALUE, Double.MIN_NORMAL);
        check(Double.isFinite(huge),
                "IEEEremainder(MAX_VALUE, MIN_NORMAL) must be finite — the quotient overflowed");
        check(Double.doubleToRawLongBits(huge) == 0x0000000000000000L,
                "IEEEremainder(MAX_VALUE, MIN_NORMAL) must be +0.0");
        System.out.println("CK RJdkStrictMath discriminators=6");
    }

    /**
     * {@code IEEEremainder} is the one function of this family that is a defect
     * in {@code Math} as well when it deviates: both classes' specs read "as
     * prescribed by the IEEE 754 standard", which fixes the result exactly, so
     * there is no 1-ULP latitude for the loose class to use. The two must
     * therefore agree BIT FOR BIT — the only place in this file where a
     * {@code Math}/{@code StrictMath} equality is a contract rather than an
     * accident.
     */
    static void mathAgreesWhereTheSpecIsExact() {
        for (int i = 0; i < REM.length; i += 3) {
            double a = Double.longBitsToDouble(REM[i]);
            double b = Double.longBitsToDouble(REM[i + 1]);
            double m = Math.IEEEremainder(a, b);
            double s = StrictMath.IEEEremainder(a, b);
            boolean ok = Double.isNaN(s) ? Double.isNaN(m)
                    : Double.doubleToRawLongBits(m) == Double.doubleToRawLongBits(s);
            check(ok, "Math.IEEEremainder and StrictMath.IEEEremainder must agree exactly"
                    + " at 0x" + Long.toHexString(REM[i]) + ",0x" + Long.toHexString(REM[i + 1]));
        }
        // `sqrt` is shared for the same kind of reason and a different one:
        // IEEE 754 requires it correctly rounded, so both classes and the
        // hardware compute one function.
        for (int i = 0; i < SQRT.length; i += 2) {
            double x = Double.longBitsToDouble(SQRT[i]);
            double m = Math.sqrt(x);
            double s = StrictMath.sqrt(x);
            boolean ok = Double.isNaN(s) ? Double.isNaN(m)
                    : Double.doubleToRawLongBits(m) == Double.doubleToRawLongBits(s);
            check(ok, "Math.sqrt and StrictMath.sqrt must agree exactly at 0x"
                    + Long.toHexString(SQRT[i]));
        }
        System.out.println("CK RJdkStrictMath exactSharedContracts=" + (REM.length / 3)
                + "+" + (SQRT.length / 2));
    }

    /**
     * {@code toRadians}/{@code toDegrees} are a single multiply by a constant
     * in JDK 25 ({@code DEGREES_TO_RADIANS = 0.017453292519943295}, which is
     * bit-identical to {@code PI/180}), NOT JDK 8's {@code angdeg / 180.0 * PI}.
     * They are exact, so they are asserted by bits rather than in a band — the
     * loose band that used to cover them is exactly the shape that let a real
     * one-ULP divergence live elsewhere in this family.
     */
    static void exactConversions() {
        check(Double.doubleToRawLongBits(StrictMath.toRadians(180.0))
                        == Double.doubleToRawLongBits(180.0 * 0.017453292519943295),
                "StrictMath.toRadians(180) must be the single-multiply result");
        check(Double.doubleToRawLongBits(StrictMath.toDegrees(Math.PI))
                        == Double.doubleToRawLongBits(Math.PI * 57.29577951308232),
                "StrictMath.toDegrees(PI) must be the single-multiply result");
        check(Double.doubleToRawLongBits(StrictMath.toRadians(0.0))
                        == Double.doubleToRawLongBits(0.0),
                "StrictMath.toRadians(0.0) must be +0.0");
        check(Double.doubleToRawLongBits(StrictMath.toRadians(-0.0))
                        == Double.doubleToRawLongBits(-0.0),
                "StrictMath.toRadians(-0.0) must be -0.0");
        // `ulp` has a single specified answer, not a small one: 2^-52 at 1.0.
        // `v > 0.0 && v < 1e-10` — the assertion this replaces — passes for
        // roughly a million wrong answers.
        check(Double.doubleToRawLongBits(StrictMath.ulp(1.0))
                        == Double.doubleToRawLongBits(0x1.0p-52),
                "StrictMath.ulp(1.0) must be exactly 2^-52");
        check(Float.floatToRawIntBits(StrictMath.ulp(1.0f))
                        == Float.floatToRawIntBits(0x1.0p-23f),
                "StrictMath.ulp(1.0f) must be exactly 2^-23");
        System.out.println("CK RJdkStrictMath exactConversions=6");
    }

    // Raw bit patterns of the two zeros, named so the assertions below read as
    // what they are. `-0.0 == 0.0` is TRUE in Java, so an equality-shaped
    // check on a signed zero passes against the defect; only the bits can see
    // the sign.
    private static final long NEG_ZERO_D = 0x8000000000000000L;
    private static final long POS_ZERO_D = 0x0000000000000000L;
    private static final int NEG_ZERO_F = 0x80000000;
    private static final int POS_ZERO_F = 0x00000000;

    // Read through arrays so javac cannot treat the operands as compile-time
    // constants and so a JIT that constant-folds `Math.min` has a second,
    // non-foldable route to get wrong. Element order: -0.0, +0.0, NaN, 1.0.
    private static final double[] OPAQUE_D = { -0.0, 0.0, Double.NaN, 1.0 };
    private static final float[] OPAQUE_F = { -0.0f, 0.0f, Float.NaN, 1.0f };

    /**
     * {@code min}/{@code max} on the two arguments the naive implementation
     * cannot see: {@code NaN} and the sign of zero.
     *
     * <p>Java fixes both exactly ({@code Math.min(double, double)}: "If either
     * value is NaN, then the result is NaN... if one argument is positive zero
     * and the other is negative zero, the result is negative zero"). Rust's
     * {@code f64::min} is IEEE {@code minNum}, which RETURNS THE NON-NaN
     * OPERAND, and {@code a < b} cannot distinguish {@code -0.0} from
     * {@code +0.0} — so a natural Rust transcription is wrong on exactly these
     * inputs and right everywhere else. Measured before the 2026-08-12 fix:
     * {@code Math.min(1.0, NaN)} was {@code 1.0} and {@code Math.min(-0.0,
     * 0.0)} was {@code +0.0}.
     *
     * <p><b>The zero rows compare RAW BITS, never {@code ==}.</b> {@code -0.0
     * == 0.0} is {@code true} in Java, so {@code min(-0.0, 0.0) == -0.0} is
     * true against the broken implementation too — an equality-shaped check
     * here is not a weak test, it is a vacuous one.
     *
     * <p><b>The pass/fail boundary is per descriptor.</b> One class name does
     * not cover another and one descriptor does not cover another: the defect
     * hit {@code Math}, {@code StrictMath}, {@code Float.min}/{@code max} (by
     * inheritance from the same registration) and {@code Double.min}/{@code
     * max} (a THIRD copy), while {@code min(II)I} and {@code min(JJ)J} were
     * correct throughout. So this block walks every class name and both
     * floating-point widths, and keeps the integral overloads as negative
     * controls — if those ever go red the cause is not this defect.
     *
     * <p>Registration is why nothing caught it: the {@code Math} registrar
     * opens with an ambient {@code NativeKind::Intrinsic}, which is exempt
     * from shadow retirement and is not the census's
     * {@code native-shadows-bytecode} kind, so a {@code --jdk-only-report} run
     * of a program calling {@code Math.min} yields zero {@code java/lang/Math}
     * rows. The census would not have found this; a vector is the only
     * instrument.
     */
    static void minMaxSpecialValues() {
        // --- NaN poisons both operand positions, double. `isNaN`, not a bit
        //     comparison: which NaN is unspecified (see this file's header).
        check(Double.isNaN(Math.min(1.0, Double.NaN)), "Math.min(1.0, NaN) must be NaN");
        check(Double.isNaN(Math.min(Double.NaN, 1.0)), "Math.min(NaN, 1.0) must be NaN");
        check(Double.isNaN(Math.max(1.0, Double.NaN)), "Math.max(1.0, NaN) must be NaN");
        check(Double.isNaN(Math.max(Double.NaN, 1.0)), "Math.max(NaN, 1.0) must be NaN");

        // --- NaN poisons both operand positions, float. A separate descriptor
        //     is a separate registration and cannot be inferred from the
        //     double rows above.
        check(Float.isNaN(Math.min(1.0f, Float.NaN)), "Math.min(1.0f, NaN) must be NaN");
        check(Float.isNaN(Math.min(Float.NaN, 1.0f)), "Math.min(NaN, 1.0f) must be NaN");
        check(Float.isNaN(Math.max(1.0f, Float.NaN)), "Math.max(1.0f, NaN) must be NaN");
        check(Float.isNaN(Math.max(Float.NaN, 1.0f)), "Math.max(NaN, 1.0f) must be NaN");

        // --- Signed zero, double, BY BITS. Both argument orders: a `<`-based
        //     body returns whichever operand the comparison happened to leave
        //     standing, so the two orders can disagree.
        check(Double.doubleToRawLongBits(Math.min(-0.0, 0.0)) == NEG_ZERO_D,
                "Math.min(-0.0, 0.0) must be -0.0 (bits 0x8000000000000000)");
        check(Double.doubleToRawLongBits(Math.min(0.0, -0.0)) == NEG_ZERO_D,
                "Math.min(0.0, -0.0) must be -0.0 (bits 0x8000000000000000)");
        check(Double.doubleToRawLongBits(Math.max(-0.0, 0.0)) == POS_ZERO_D,
                "Math.max(-0.0, 0.0) must be +0.0 (bits 0x0)");
        check(Double.doubleToRawLongBits(Math.max(0.0, -0.0)) == POS_ZERO_D,
                "Math.max(0.0, -0.0) must be +0.0 (bits 0x0)");

        // --- Signed zero, float, BY BITS.
        check(Float.floatToRawIntBits(Math.min(-0.0f, 0.0f)) == NEG_ZERO_F,
                "Math.min(-0.0f, 0.0f) must be -0.0f (bits 0x80000000)");
        check(Float.floatToRawIntBits(Math.min(0.0f, -0.0f)) == NEG_ZERO_F,
                "Math.min(0.0f, -0.0f) must be -0.0f (bits 0x80000000)");
        check(Float.floatToRawIntBits(Math.max(-0.0f, 0.0f)) == POS_ZERO_F,
                "Math.max(-0.0f, 0.0f) must be +0.0f (bits 0x0)");
        check(Float.floatToRawIntBits(Math.max(0.0f, -0.0f)) == POS_ZERO_F,
                "Math.max(0.0f, -0.0f) must be +0.0f (bits 0x0)");

        // --- `StrictMath` is a SECOND class name over the same contract; its
        //     min/max are specified identically to Math's, so a fix applied to
        //     one registrar and not the other shows up here and nowhere else.
        check(Double.isNaN(StrictMath.min(1.0, Double.NaN)),
                "StrictMath.min(1.0, NaN) must be NaN");
        check(Double.doubleToRawLongBits(StrictMath.min(-0.0, 0.0)) == NEG_ZERO_D,
                "StrictMath.min(-0.0, 0.0) must be -0.0");
        check(Float.isNaN(StrictMath.max(Float.NaN, 1.0f)),
                "StrictMath.max(NaN, 1.0f) must be NaN");
        check(Float.floatToRawIntBits(StrictMath.max(-0.0f, 0.0f)) == POS_ZERO_F,
                "StrictMath.max(-0.0f, 0.0f) must be +0.0f");

        // --- `Double.min`/`max` are a THIRD copy of the same body, reached by
        //     a different owner class, and were wrong independently.
        check(Double.isNaN(Double.min(1.0, Double.NaN)),
                "Double.min(1.0, NaN) must be NaN");
        check(Double.doubleToRawLongBits(Double.min(-0.0, 0.0)) == NEG_ZERO_D,
                "Double.min(-0.0, 0.0) must be -0.0");
        check(Double.doubleToRawLongBits(Double.max(-0.0, 0.0)) == POS_ZERO_D,
                "Double.max(-0.0, 0.0) must be +0.0");

        // --- `Float.min`/`max`, the fourth owner.
        check(Float.isNaN(Float.min(1.0f, Float.NaN)),
                "Float.min(1.0f, NaN) must be NaN");
        check(Float.floatToRawIntBits(Float.min(-0.0f, 0.0f)) == NEG_ZERO_F,
                "Float.min(-0.0f, 0.0f) must be -0.0f");

        // --- Non-constant operands. Everything above is a literal pair, which
        //     a constant-folding JIT may answer without ever entering the
        //     implementation under test; these come out of an array.
        double negZeroD = OPAQUE_D[0];
        double posZeroD = OPAQUE_D[1];
        double nanD = OPAQUE_D[2];
        double oneD = OPAQUE_D[3];
        check(Double.doubleToRawLongBits(Math.min(negZeroD, posZeroD)) == NEG_ZERO_D,
                "Math.min(-0.0, 0.0) must be -0.0 for non-constant operands");
        check(Double.doubleToRawLongBits(Math.max(negZeroD, posZeroD)) == POS_ZERO_D,
                "Math.max(-0.0, 0.0) must be +0.0 for non-constant operands");
        check(Double.isNaN(Math.min(oneD, nanD)),
                "Math.min(1.0, NaN) must be NaN for non-constant operands");
        check(Float.floatToRawIntBits(Math.min(OPAQUE_F[0], OPAQUE_F[1])) == NEG_ZERO_F,
                "Math.min(-0.0f, 0.0f) must be -0.0f for non-constant operands");

        // --- NEGATIVE CONTROLS. The integral descriptors were correct before
        //     the fix and must stay correct after it; a failure here is a
        //     different defect wearing this block's name.
        check(Math.min(1, 2) == 1, "Math.min(1, 2) must be 1");
        check(Math.max(1, 2) == 2, "Math.max(1, 2) must be 2");
        check(Math.min(-3L, 2L) == -3L, "Math.min(-3L, 2L) must be -3");
        check(Math.max(-3L, 2L) == 2L, "Math.max(-3L, 2L) must be 2");

        System.out.println("CK RJdkStrictMath minMax=33");
    }

    public static void main(String[] args) {
        fdlibmFamily();
        namedDiscriminators();
        mathAgreesWhereTheSpecIsExact();
        exactConversions();
        minMaxSpecialValues();
        System.out.println("CK RJdkStrictMath checks=" + checks);
        System.out.println("PASS RJdkStrictMath (" + checks + " checks)");
    }
}
