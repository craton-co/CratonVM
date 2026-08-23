// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigDecimal;

/**
 * `Math.fma` / `StrictMath.fma`, on the inputs where a fused multiply-add is
 * not the same thing as `a * b + c`.
 *
 * CratonVM answers these from Rust's `f64::mul_add` / `f32::mul_add` rather
 * than running the JDK's Java fallback, which builds two `BigDecimal`s and
 * does a `BigInteger` Knuth division per call — measured at 28.6% of
 * GPULlama3's whole inference kernel, because `FloatVector.fma` calls it per
 * lane.
 *
 * Rust's `mul_add` and the JDK's fallback both claim IEEE 754
 * `fusedMultiplyAdd`, so every row here must agree BIT FOR BIT with the
 * oracle. Bits are printed, not values: `-0.0` and `0.0` print the same and
 * are different answers, and a NaN payload difference is invisible as text.
 *
 * The rows are chosen to separate fused from unfused, which a row like
 * `fma(2,3,4)` cannot:
 *
 *   * `x*x - y*y` shapes where the unrounded product carries bits the
 *     single-rounded product loses;
 *   * a product that OVERFLOWS to infinity but whose fused sum is finite;
 *   * a product that UNDERFLOWS to zero but whose fused sum is not;
 *   * the specified NaN cases: any NaN argument, and infinity times zero;
 *   * signed zeros, whose sign the addition decides.
 *
 * The `NAIVE` column is `a * b + c` computed in Java, printed so a reader can
 * see which rows the test would pass by accident if `fma` were implemented as
 * a plain multiply-add. Rows where FUSED and NAIVE agree prove nothing.
 */
public class MathFmaProbe {

    /**
     * A `double` as raw bits, except that every NaN prints as `NaN`.
     *
     * `-0.0` and `0.0` must still be distinguishable — the addition decides the
     * sign of a zero result and that IS specified — so everything finite keeps
     * its bit pattern.
     */
    static String canon(double v) {
        return Double.isNaN(v) ? "NaN" : Long.toString(Double.doubleToRawLongBits(v));
    }

    static String canon(float v) {
        return Float.isNaN(v) ? "NaN" : Integer.toString(Float.floatToRawIntBits(v));
    }

    static final double[][] D_ROWS = {
        { 2.0, 3.0, 4.0 },
        { 0.1, 0.1, -0.01 },
        { 1.0000000000000002, 1.0000000000000002, -1.0000000000000004 },
        { 1e300, 1e300, Double.NEGATIVE_INFINITY },
        { 1e300, 1e300, -1e300 },
        { 1e-300, 1e-300, 1.0 },
        { 1e-300, 1e-300, 0.0 },
        { Double.MAX_VALUE, 2.0, Double.NEGATIVE_INFINITY },
        { Double.MIN_VALUE, 0.5, 0.0 },
        { -0.0, 0.0, -0.0 },
        { -0.0, 0.0, 0.0 },
        { 0.0, Double.POSITIVE_INFINITY, 1.0 },
        { Double.POSITIVE_INFINITY, 0.0, 1.0 },
        { Double.NaN, 1.0, 1.0 },
        { 1.0, Double.NaN, 1.0 },
        { 1.0, 1.0, Double.NaN },
        { Double.POSITIVE_INFINITY, 2.0, Double.NEGATIVE_INFINITY },
        { -1.0, Double.POSITIVE_INFINITY, Double.POSITIVE_INFINITY },
        { 3.0, 1.0 / 3.0, -1.0 },
        { 1.4142135623730951, 1.4142135623730951, -2.0 },
    };

    static final float[][] F_ROWS = {
        { 2.0f, 3.0f, 4.0f },
        { 0.1f, 0.1f, -0.01f },
        { 1.0000001f, 1.0000001f, -1.0000002f },
        { 1e38f, 1e38f, Float.NEGATIVE_INFINITY },
        { 1e38f, 1e38f, -1e38f },
        { 1e-38f, 1e-38f, 1.0f },
        { 1e-38f, 1e-38f, 0.0f },
        { Float.MAX_VALUE, 2.0f, Float.NEGATIVE_INFINITY },
        { Float.MIN_VALUE, 0.5f, 0.0f },
        { -0.0f, 0.0f, -0.0f },
        { -0.0f, 0.0f, 0.0f },
        { 0.0f, Float.POSITIVE_INFINITY, 1.0f },
        { Float.POSITIVE_INFINITY, 0.0f, 1.0f },
        { Float.NaN, 1.0f, 1.0f },
        { 1.0f, Float.NaN, 1.0f },
        { 1.0f, 1.0f, Float.NaN },
        { Float.POSITIVE_INFINITY, 2.0f, Float.NEGATIVE_INFINITY },
        { 3.0f, 1.0f / 3.0f, -1.0f },
        { 1.4142135f, 1.4142135f, -2.0f },
    };

    public static void main(String[] args) {
        int checks = 0;
        for (double[] r : D_ROWS) {
            double fused = Math.fma(r[0], r[1], r[2]);
            double strict = StrictMath.fma(r[0], r[1], r[2]);
            double naive = r[0] * r[1] + r[2];
            // VALUE columns: a NaN is printed as `NaN`, not as its bits. That
            // is the diffable comparison, because a NaN's sign and payload are
            // unspecified in Java and CratonVM's `Value::Double` cannot carry
            // them across a native boundary at all (the CompactValue tag
            // collision, `nan-payloads-lost-to-the-compactvalue-tag-collision`).
            System.out.println("FMA-D " + canon(r[0]) + " " + canon(r[1]) + " " + canon(r[2])
                    + " FUSED=" + canon(fused)
                    + " STRICT=" + canon(strict)
                    + " NAIVE=" + canon(naive));
            // BITS: printed separately and NOT compared, so the payload
            // divergence stays visible to a reader without failing the vector.
            System.out.println("FMA-D-BITS " + Double.doubleToRawLongBits(fused)
                    + " " + Double.doubleToRawLongBits(strict)
                    + " " + Double.doubleToRawLongBits(naive));
            checks += 3;
        }
        for (float[] r : F_ROWS) {
            float fused = Math.fma(r[0], r[1], r[2]);
            float strict = StrictMath.fma(r[0], r[1], r[2]);
            float naive = r[0] * r[1] + r[2];
            System.out.println("FMA-F " + canon(r[0]) + " " + canon(r[1]) + " " + canon(r[2])
                    + " FUSED=" + canon(fused)
                    + " STRICT=" + canon(strict)
                    + " NAIVE=" + canon(naive));
            System.out.println("FMA-F-BITS " + Float.floatToRawIntBits(fused)
                    + " " + Float.floatToRawIntBits(strict)
                    + " " + Float.floatToRawIntBits(naive));
            checks += 3;
        }

        // An independent oracle for the finite rows, computed the way the JDK's
        // own fallback computes it. This is what makes the probe more than a
        // "CratonVM agrees with CratonVM" comparison when it is read alone.
        int mismatches = 0;
        for (double[] r : D_ROWS) {
            if (!Double.isFinite(r[0]) || !Double.isFinite(r[1]) || !Double.isFinite(r[2])
                    || r[0] == 0.0 || r[1] == 0.0) {
                continue;
            }
            double exact = new BigDecimal(r[0]).multiply(new BigDecimal(r[1]))
                    .add(new BigDecimal(r[2])).doubleValue();
            double fused = Math.fma(r[0], r[1], r[2]);
            checks++;
            if (Double.doubleToRawLongBits(exact) != Double.doubleToRawLongBits(fused)) {
                mismatches++;
                System.out.println("FMA-BIGDEC-MISMATCH a=" + r[0] + " b=" + r[1] + " c=" + r[2]
                        + " bigdec=" + Double.doubleToRawLongBits(exact)
                        + " fused=" + Double.doubleToRawLongBits(fused));
            }
        }
        System.out.println("FMA bigdecimal_mismatches=" + mismatches);
        System.out.println("CK MathFmaProbe checks=" + checks);
        System.out.println(mismatches == 0 ? "PASS MathFmaProbe" : "FAIL MathFmaProbe");
    }
}
