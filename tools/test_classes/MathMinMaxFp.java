// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * `Math.min`/`Math.max` for float and double against every semantic corner
 * the SSE MIN/MAX instructions get wrong.
 *
 * The JIT lowers these four to an inline SSE sequence rather than the JDK's
 * Java body. MINSS/MAXSS on their own are NOT Math.min/Math.max: the SDM
 * says `MINSS dst, src` returns `src` whenever both operands are zero or
 * either is NaN, while Java requires
 *
 *   * a NaN result whenever either argument is NaN, and specifically the
 *     NaN ARGUMENT, whose payload is observable via floatToRawIntBits; and
 *   * -0.0 to be strictly smaller than +0.0.
 *
 * Every assertion below is checked against raw bits, not `==`, because
 * `-0.0 == 0.0` is true and `NaN == NaN` is false — neither would notice
 * the two bugs this fixture exists to catch. Each case is driven through a
 * loop hot enough to compile, so the interpreter's answer and the compiled
 * answer are both exercised and must agree with the constants below.
 */
public class MathMinMaxFp {

    static final int NEG_ZERO_F = Float.floatToRawIntBits(-0.0f);
    static final int POS_ZERO_F = Float.floatToRawIntBits(0.0f);
    static final long NEG_ZERO_D = Double.doubleToRawLongBits(-0.0d);
    static final long POS_ZERO_D = Double.doubleToRawLongBits(0.0d);

    // Two DISTINCT NaN payloads, so "returned the right argument" is
    // distinguishable from "returned some NaN".
    static final float NAN_A_F = Float.intBitsToFloat(0x7FC00001);
    static final float NAN_B_F = Float.intBitsToFloat(0x7FC00002);
    static final double NAN_A_D = Double.longBitsToDouble(0x7FF8000000000001L);
    static final double NAN_B_D = Double.longBitsToDouble(0x7FF8000000000002L);

    static float minF(float a, float b) { return Math.min(a, b); }
    static float maxF(float a, float b) { return Math.max(a, b); }
    static double minD(double a, double b) { return Math.min(a, b); }
    static double maxD(double a, double b) { return Math.max(a, b); }

    static int failures = 0;

    static void eqF(String what, float got, float want) {
        int g = Float.floatToRawIntBits(got);
        int w = Float.floatToRawIntBits(want);
        if (g != w) {
            System.out.println("FAIL " + what + " got=0x" + Integer.toHexString(g)
                    + " want=0x" + Integer.toHexString(w));
            failures++;
        }
    }

    static void eqD(String what, double got, double want) {
        long g = Double.doubleToRawLongBits(got);
        long w = Double.doubleToRawLongBits(want);
        if (g != w) {
            System.out.println("FAIL " + what + " got=0x" + Long.toHexString(g)
                    + " want=0x" + Long.toHexString(w));
            failures++;
        }
    }

    static void round() {
        // --- ordinary ordering ---
        eqF("minF(3,5)", minF(3f, 5f), 3f);
        eqF("minF(5,3)", minF(5f, 3f), 3f);
        eqF("maxF(3,5)", maxF(3f, 5f), 5f);
        eqF("maxF(5,3)", maxF(5f, 3f), 5f);
        eqF("minF(-7,4)", minF(-7f, 4f), -7f);
        eqF("maxF(-7,4)", maxF(-7f, 4f), 4f);
        eqF("minF(2,2)", minF(2f, 2f), 2f);

        // --- signed zero: -0.0 is strictly smaller than +0.0 ---
        eqF("minF(+0,-0)", minF(0.0f, -0.0f), -0.0f);
        eqF("minF(-0,+0)", minF(-0.0f, 0.0f), -0.0f);
        eqF("minF(-0,-0)", minF(-0.0f, -0.0f), -0.0f);
        eqF("minF(+0,+0)", minF(0.0f, 0.0f), 0.0f);
        eqF("maxF(+0,-0)", maxF(0.0f, -0.0f), 0.0f);
        eqF("maxF(-0,+0)", maxF(-0.0f, 0.0f), 0.0f);
        eqF("maxF(-0,-0)", maxF(-0.0f, -0.0f), -0.0f);
        eqF("maxF(+0,+0)", maxF(0.0f, 0.0f), 0.0f);
        // A zero against a non-zero must not go through the zero path.
        eqF("minF(-0,1)", minF(-0.0f, 1f), -0.0f);
        eqF("minF(1,-0)", minF(1f, -0.0f), -0.0f);
        eqF("maxF(-0,-1)", maxF(-0.0f, -1f), -0.0f);

        // --- NaN: the result is the NaN ARGUMENT, payload and all ---
        eqF("minF(NaN,5)", minF(NAN_A_F, 5f), NAN_A_F);
        eqF("minF(5,NaN)", minF(5f, NAN_B_F), NAN_B_F);
        eqF("maxF(NaN,5)", maxF(NAN_A_F, 5f), NAN_A_F);
        eqF("maxF(5,NaN)", maxF(5f, NAN_B_F), NAN_B_F);
        // Both NaN: the JDK body's first test is `a != a`, so `a` wins.
        eqF("minF(NaNa,NaNb)", minF(NAN_A_F, NAN_B_F), NAN_A_F);
        eqF("maxF(NaNa,NaNb)", maxF(NAN_A_F, NAN_B_F), NAN_A_F);

        // --- infinities ---
        eqF("minF(-inf,0)", minF(Float.NEGATIVE_INFINITY, 0f), Float.NEGATIVE_INFINITY);
        eqF("maxF(inf,0)", maxF(Float.POSITIVE_INFINITY, 0f), Float.POSITIVE_INFINITY);
        eqF("minF(inf,NaN)", minF(Float.POSITIVE_INFINITY, NAN_A_F), NAN_A_F);

        // --- the same grid for double ---
        eqD("minD(3,5)", minD(3d, 5d), 3d);
        eqD("maxD(5,3)", maxD(5d, 3d), 5d);
        eqD("minD(-7,4)", minD(-7d, 4d), -7d);
        eqD("minD(+0,-0)", minD(0.0d, -0.0d), -0.0d);
        eqD("minD(-0,+0)", minD(-0.0d, 0.0d), -0.0d);
        eqD("maxD(+0,-0)", maxD(0.0d, -0.0d), 0.0d);
        eqD("maxD(-0,+0)", maxD(-0.0d, 0.0d), 0.0d);
        eqD("maxD(-0,-0)", maxD(-0.0d, -0.0d), -0.0d);
        eqD("minD(NaN,5)", minD(NAN_A_D, 5d), NAN_A_D);
        eqD("minD(5,NaN)", minD(5d, NAN_B_D), NAN_B_D);
        eqD("maxD(NaN,5)", maxD(NAN_A_D, 5d), NAN_A_D);
        eqD("maxD(5,NaN)", maxD(5d, NAN_B_D), NAN_B_D);
        eqD("minD(-inf,0)", minD(Double.NEGATIVE_INFINITY, 0d), Double.NEGATIVE_INFINITY);
        eqD("maxD(inf,0)", maxD(Double.POSITIVE_INFINITY, 0d), Double.POSITIVE_INFINITY);
    }

    public static void main(String[] args) {
        // Cold rounds run interpreted; later rounds run compiled. Both must
        // give the same answers, which is the point of looping rather than
        // calling `round()` once.
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40000;
        for (int i = 0; i < rounds; i++) {
            round();
            if (failures > 50) break; // don't drown the log
        }
        // A running sum keeps the calls from being optimised away entirely
        // and gives a second, coarser agreement check.
        float fs = 0f;
        double ds = 0d;
        for (int i = 0; i < rounds; i++) {
            fs += Math.min(i * 0.5f, 1000f) + Math.max(i * -0.25f, -500f);
            ds += Math.min(i * 0.5d, 1000d) + Math.max(i * -0.25d, -500d);
        }
        System.out.println("MATHMINMAXFP failures=" + failures
                + " fsum=" + Float.floatToRawIntBits(fs)
                + " dsum=" + Double.doubleToRawLongBits(ds));
    }
}
