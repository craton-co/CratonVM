// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigDecimal;
import java.math.BigInteger;

/**
 * Per-call cost of the CHEAPEST methods on the bignum native surface, against a
 * control that is an ordinary one-line Java getter.
 *
 * <p>Why this shape. {@code BigDecimalBench} measures the whole 60-digit
 * expression and cannot say whether a given method is expensive because of what
 * it computes or because of what it costs to call. These four all read state
 * that is already stored — {@code signum}, {@code scale}, {@code precision} are
 * a field read on any conforming JDK — so anything above the control's number
 * is overhead, and any ONE of them standing well above the others is a defect
 * in that method rather than in the dispatch path.
 *
 * <p>That is exactly how {@code BigDecimal.signum()} was caught: 738 ns/call
 * against 148-153 ns for its siblings, because it rendered the whole magnitude
 * to a decimal {@code String} to read the first byte.
 *
 * <p>Every loop is monomorphic and allocation-free — no lambda, no
 * {@code Runnable} — because a SAM dispatch in the timed loop measures the
 * lambda, not the callee (see
 * {@code perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817}).
 *
 * <pre>
 *   java          -cp &lt;dir&gt; BignumNativeCostProbe [iterations]
 *   &lt;cratonvm&gt; --java-home &lt;jdk&gt; -cp &lt;dir&gt; BignumNativeCostProbe [iterations]
 * </pre>
 */
public final class BignumNativeCostProbe {

    /** The control: a one-line getter on an ordinary class, same call shape. */
    static final class Control {
        private final int field = 3;

        int read() {
            return field;
        }
    }

    private static long sink;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;

        BigInteger big = new BigInteger(
                "123456789012345678901234567890123456789012345678901234567890");
        BigDecimal dec = new BigDecimal("1.23456789012345678901234567890123456789");
        Control control = new Control();

        // Correctness gate before any timing.
        if (big.signum() != 1 || dec.signum() != 1 || dec.scale() != 38
                || dec.precision() != 39 || control.read() != 3) {
            throw new AssertionError("bignum accessors disagree with the JDK contract: "
                    + big.signum() + " " + dec.signum() + " " + dec.scale() + " "
                    + dec.precision());
        }

        for (int round = 0; round < 2; round++) {
            long t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += control.read();
            }
            report("Control.read() [control]", t, n);

            t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += big.signum();
            }
            report("BigInteger.signum()", t, n);

            t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += dec.scale();
            }
            report("BigDecimal.scale()", t, n);

            t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += dec.signum();
            }
            report("BigDecimal.signum()", t, n);

            t = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += dec.precision();
            }
            report("BigDecimal.precision()", t, n);

            System.out.println(round == 0 ? "---- warmup above ----" : "---- done ----");
        }
        System.out.println("sink=" + sink);
    }

    private static void report(String name, long startNanos, int n) {
        long ns = (System.nanoTime() - startNanos) / n;
        System.out.printf("%-26s %6d ns/call%n", name, ns);
    }
}
