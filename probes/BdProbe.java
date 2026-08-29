// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigDecimal;
import java.math.BigInteger;

/**
 * RBIGDEC.1 fixture — the BigInteger/BigDecimal {@code <clinit>} recovery probe.
 *
 * <p>Restored 2026-08-07. It previously lived at {@code apps/bigdecimal_probe/BdProbe.java},
 * which {@code .gitignore} line 12 ({@code apps/}) makes untracked — so it was never in the
 * repository and both RBIGDEC.1 harnesses
 * ({@code vm/tests/rbigdec1_arithmetic.rs}, {@code vm/tests/rbigdec1_full_arithmetic.rs})
 * skipped and reported {@code ok} in 0.00 s while asserting nothing. {@code probes/} is the
 * tracked home; do not move this back under {@code apps/}.
 *
 * <p>Every line below is pinned by those two harnesses. Changing the output breaks them:
 *
 * <pre>
 *   11      &lt;- BigDecimal.ONE.add(BigDecimal.TEN)      (scale 0, so "11" not "11.0")
 *   20      &lt;- BigInteger.TWO.multiply(BigInteger.TEN)
 *   OK      &lt;- reached the end without the KC16 cascade NPE
 * </pre>
 *
 * <p>What it exists to catch: {@code java/math/BigInteger.<clinit>} and
 * {@code java/math/BigDecimal.<clinit>} used to silently swallow in real-JDK mode, leaving the
 * static constants null. The observable symptom was
 * "Cannot read field 'signum' because the object is null" — which
 * {@code rbigdec1_arithmetic.rs} asserts is absent from stdout. The constants are therefore
 * read through the public {@code ONE}/{@code TEN}/{@code TWO} fields on purpose: constructing
 * the values with {@code valueOf} would route around the exact {@code <clinit>} path under test.
 *
 * <p>Printing goes through {@code toString()} on the result rather than a formatted string, so a
 * VM that returns a correctly-valued object with a broken {@code toString} still fails here.
 */
public class BdProbe {
    public static void main(String[] args) {
        // BigDecimal: both operands come from the class's own static constants,
        // so a <clinit> that left them null NPEs right here.
        BigDecimal sum = BigDecimal.ONE.add(BigDecimal.TEN);
        System.out.println(sum.toString());

        // BigInteger: same shape, different class — the two <clinit>s failed
        // independently, and BigDecimal's cascades into BigInteger's.
        BigInteger product = BigInteger.TWO.multiply(BigInteger.TEN);
        System.out.println(product.toString());

        // Self-check before the OK marker: if the arithmetic silently produced
        // the wrong value the lines above would still print, so assert here and
        // exit non-zero rather than letting a wrong answer reach "OK".
        if (!"11".equals(sum.toString()) || !"20".equals(product.toString())) {
            System.out.println("FAIL sum=" + sum + " product=" + product);
            System.exit(1);
        }

        System.out.println("OK");
    }
}
