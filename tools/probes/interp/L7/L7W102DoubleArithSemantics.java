// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L7: CRATONVM_INTERP_FUSED_DOUBLE_ARITH
// must answer exactly as the unfused `dload; dload; d<op> [; dstore]` and
// `dload; dload; dcmpl | dcmpg` arms: IEEE results bit for bit (NaN payload
// propagation printed as raw bits, -0.0, infinities, `drem`'s fmod), and
// `dcmpl` -1 / `dcmpg` 1 for a NaN. Run it with the switch off and on (and
// with the JIT on and `--nojit`); every run prints the same lines as HotSpot
// 25 (the orchestrator records them).
// Run: cratonvm --java-home <jdk25> [--nojit] -cp <dir> L7W102DoubleArithSemantics
//      CRATONVM_INTERP_FUSED_DOUBLE_ARITH=1 cratonvm ... L7W102DoubleArithSemantics
public class L7W102DoubleArithSemantics {

    // Each body is `dload_0; dload_2; <op>; dreturn` (push form).
    static double add(double a, double b) {
        return a + b;
    }

    static double sub(double a, double b) {
        return a - b;
    }

    static double mul(double a, double b) {
        return a * b;
    }

    static double div(double a, double b) {
        return a / b;
    }

    static double rem(double a, double b) {
        return a % b;
    }

    // `dload_0; dload_2; dcmpg; ifge` and `dload_0; dload_2; dcmpl; ifle`.
    static int lt(double a, double b) {
        return a < b ? 1 : 0;
    }

    static int gt(double a, double b) {
        return a > b ? 1 : 0;
    }

    // Store forms: `dload_0; dload_2; d<op>; dstore 4` / `dstore_0`, and the
    // constant operand `dconst_1`.
    static double storeForms(double a, double b) {
        double c = a * b;
        a = a - b;
        double d = c + 1.0;
        return a + c + d;
    }

    static float frem(float a, float b) {
        return a % b;
    }

    static String fbits(float f) {
        return Float.toString(f) + "/" + Integer.toHexString(Float.floatToRawIntBits(f));
    }

    static String bits(double d) {
        return Double.toString(d) + "/" + Long.toHexString(Double.doubleToRawLongBits(d));
    }

    public static void main(String[] args) {
        double[] vs = {
            0.0, -0.0, 1.0, -1.5, 3.0, 1e308, -1e308, Double.MIN_VALUE,
            Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
            Double.longBitsToDouble(0x7ff8_0000_0000_1234L),
        };
        for (double a : vs) {
            for (double b : vs) {
                System.out.println(bits(a) + " " + bits(b)
                        + " add=" + bits(add(a, b))
                        + " sub=" + bits(sub(a, b))
                        + " mul=" + bits(mul(a, b))
                        + " div=" + bits(div(a, b))
                        + " rem=" + bits(rem(a, b))
                        + " lt=" + lt(a, b)
                        + " gt=" + gt(a, b)
                        + " st=" + bits(storeForms(a, b)));
            }
        }
        // `frem` over the float edge values (hand-back w102: the two-NaN
        // remainder keeps the divisor's NaN on Linux HotSpot).
        float[] fs = {
            0.0f, -0.0f, 1.0f, -1.5f, Float.MIN_VALUE, Float.POSITIVE_INFINITY,
            Float.NaN, Float.intBitsToFloat(0x7fc0_1234),
        };
        for (float a : fs) {
            for (float b : fs) {
                System.out.println("f " + fbits(a) + " " + fbits(b) + " rem=" + fbits(frem(a, b)));
            }
        }
    }
}
