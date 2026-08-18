// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.MathContext;
import java.math.RoundingMode;

/**
 * Transcript verifier for the bignum boundaries that carry a sign or a
 * magnitude round trip: {@code signum} / {@code negate} / {@code abs} on both
 * {@link BigInteger} and {@link BigDecimal}, plus the {@code mag:[I} read/write
 * path every arithmetic result goes through.
 *
 * <p>These natives were rewritten on 2026-08-18 to stop rendering the magnitude
 * to a decimal {@code String} (see
 * {@code perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817}).
 * The old implementations were exact, so the replacement has to be exact in the
 * same places — and the interesting places are the ones a decimal rendering
 * papered over: the compact/inflated split at {@code |unscaled| > Long.MAX_VALUE},
 * {@code Long.MIN_VALUE} (which must stay INFLATED, it is the sentinel), zero
 * with a non-zero scale, and negative scales, whose plain rendering bakes in
 * trailing zeros.
 *
 * <p>It prints a transcript rather than asserting, so the check is a diff
 * against a real JDK:
 *
 * <pre>
 *   java          -cp &lt;dir&gt; BignumSignRoundTripProbe &gt; hotspot.txt
 *   &lt;cratonvm&gt; --java-home &lt;jdk&gt; -cp &lt;dir&gt; BignumSignRoundTripProbe &gt; cratonvm.txt
 *   diff hotspot.txt cratonvm.txt   # must be empty
 * </pre>
 */
public final class BignumSignRoundTripProbe {

    private static final String[] INTEGERS = {
        "0",
        "1",
        "-1",
        "9223372036854775807",          // Long.MAX_VALUE
        "-9223372036854775807",
        "9223372036854775808",          // one past — forces inflation
        "-9223372036854775808",         // Long.MIN_VALUE: the INFLATED sentinel
        "-9223372036854775809",
        "18446744073709551616",         // 2^64, exactly two limbs
        "4294967296",                   // 2^32, limb boundary
        "-4294967296",
        "123456789012345678901234567890123456789012345678901234567890",
        "-123456789012345678901234567890123456789012345678901234567890",
        "1000000000000000000000000000000000000000000000000000000000000",
    };

    private static final int[] SCALES = {0, 1, 7, -3, 38, -38};

    public static void main(String[] args) {
        for (String text : INTEGERS) {
            BigInteger i = new BigInteger(text);
            line("BI", text, "signum", i.signum());
            line("BI", text, "negate", i.negate());
            line("BI", text, "abs", i.abs());
            line("BI", text, "negate.negate", i.negate().negate());
            line("BI", text, "abs.negate", i.abs().negate());
            line("BI", text, "bitLength", i.bitLength());
            line("BI", text, "toString", i.toString());
            // The mag[] round trip: a result object is written by the native
            // and read back by the next one.
            line("BI", text, "x1.roundtrip", i.multiply(BigInteger.ONE));
            line("BI", text, "plus0", i.add(BigInteger.ZERO));
            line("BI", text, "sq.sign", i.multiply(i).signum());
        }

        for (String text : INTEGERS) {
            for (int scale : SCALES) {
                BigDecimal d = new BigDecimal(new BigInteger(text), scale);
                String key = text + "@" + scale;
                line("BD", key, "signum", d.signum());
                line("BD", key, "negate", d.negate());
                line("BD", key, "abs", d.abs());
                line("BD", key, "negate.signum", d.negate().signum());
                line("BD", key, "abs.signum", d.abs().signum());
                line("BD", key, "scale", d.scale());
                line("BD", key, "precision", d.precision());
                line("BD", key, "unscaled", d.unscaledValue());
                line("BD", key, "toString", d.toString());
                line("BD", key, "compareTo0", d.compareTo(BigDecimal.ZERO));
                line("BD", key, "negate.compareTo", d.negate().compareTo(d));
            }
        }

        // Rounded arithmetic on the shape LegendreHighPrecisionTest uses, so a
        // wrong limb path shows up as a wrong digit rather than only as a sign.
        MathContext mc = new MathContext(60);
        BigDecimal a = new BigDecimal("1.23456789012345678901234567890123456789", mc);
        BigDecimal b = new BigDecimal("-9.87654321098765432109876543210987654321", mc);
        line("MC", "a*b", "value", a.multiply(b, mc));
        line("MC", "a/b", "value", a.divide(b, mc));
        line("MC", "a+b", "value", a.add(b, mc));
        line("MC", "a-b", "value", a.subtract(b, mc));
        line("MC", "(a*b).abs", "value", a.multiply(b, mc).abs());
        line("MC", "(a/b).negate", "value", a.divide(b, mc).negate());
        for (RoundingMode mode : RoundingMode.values()) {
            if (mode == RoundingMode.UNNECESSARY) {
                continue;
            }
            line("RM", mode.name(), "setScale20", b.setScale(20, mode));
            line("RM", mode.name(), "setScale0", b.setScale(0, mode));
        }
        System.out.println("TRANSCRIPT-END");
    }

    private static void line(String kind, String subject, String op, Object value) {
        System.out.println(kind + "\t" + subject + "\t" + op + "\t" + value);
    }
}
