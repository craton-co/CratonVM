// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigDecimal;
import java.math.MathContext;

/**
 * The isolated repro behind
 * {@code perf/bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817}.
 *
 * That page described this benchmark inline and called it "trivial to
 * recreate", which is how a repro stops existing: the next person retypes it
 * slightly differently and the numbers stop being comparable. It is a file now.
 *
 * <p>60-digit {@link MathContext} arithmetic — the shape
 * {@code LegendreHighPrecisionTest} spends its whole runtime in. Operands are
 * ~200 bits, far too small for a Karatsuba-vs-schoolbook complexity difference
 * to explain the measured gap; HotSpot uses schoolbook at this size too.
 *
 * <p>Prints a checksum before the timing so a build that is fast because it is
 * WRONG fails loudly instead of posting a good number.
 *
 * <pre>
 *   java          -cp &lt;dir&gt; BigDecimalBench [iterations]
 *   &lt;cratonvm&gt; --java-home &lt;jdk&gt; -cp &lt;dir&gt; BigDecimalBench [iterations]
 * </pre>
 */
public final class BigDecimalBench {

    private static final MathContext MC = new MathContext(60);

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;

        BigDecimal a = new BigDecimal("1.23456789012345678901234567890123456789", MC);
        BigDecimal b = new BigDecimal("9.87654321098765432109876543210987654321", MC);

        // Correctness gate: one iteration's worth of algebra, checked against
        // values every conforming JDK agrees on. A VM that answers wrongly here
        // must not go on to report a time.
        BigDecimal probeMul = a.multiply(b, MC);
        BigDecimal probeDiv = a.divide(b, MC);
        if (probeMul.precision() > 60 || probeDiv.precision() > 60) {
            throw new AssertionError("MathContext(60) not honoured: "
                    + probeMul.precision() + "/" + probeDiv.precision());
        }
        // (a*b)/b == a to 60 significant digits.
        BigDecimal roundTrip = probeMul.divide(b, MC);
        if (roundTrip.subtract(a, MC).abs().compareTo(new BigDecimal("1e-58")) > 0) {
            throw new AssertionError("(a*b)/b != a : " + roundTrip);
        }

        // Warm up so the timed loop measures steady state on both VMs.
        run(a, b, Math.min(iterations, 20_000));

        long t0 = System.nanoTime();
        BigDecimal acc = run(a, b, iterations);
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        // The accumulator is printed, not just computed, so it cannot be
        // optimised away and so two VMs can be diffed on the VALUE as well as
        // the time.
        System.out.println("checksum " + acc.round(new MathContext(30)));
        System.out.println("BigDecimalBench " + iterations + " iterations: " + ms + " ms");
    }

    private static BigDecimal run(BigDecimal a, BigDecimal b, int iterations) {
        BigDecimal acc = BigDecimal.ZERO;
        for (int i = 0; i < iterations; i++) {
            BigDecimal x = a.multiply(b, MC).add(a.divide(b, MC), MC).subtract(b, MC);
            acc = acc.add(x, MC);
        }
        return acc;
    }
}
