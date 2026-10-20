// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The loop bound written inline as `arr.length` rather than hoisted into
 * a local first.
 *
 * javac compiles `for (int i = 0; i < out.length; i++)` to
 *
 *     iload iv; aload out; arraylength; if_icmpge exit
 *
 * where the hoisted form (`int n = out.length; ... i < n`) compiles to
 *
 *     iload iv; iload n; if_icmpge exit
 *
 * The loop recognizer only accepted the second shape, so the first —
 * which is what most people write, and what every JDK collection idiom
 * looks like — fell back to the CPU with no diagnostic beyond a
 * `--print-gpu-decisions` line. Both resolve to the same array
 * parameter's `pN_len` kernel argument, so accepting the inline form
 * costs nothing in the emitter.
 *
 * `scaleInline` and `scaleHoisted` are the same computation written both
 * ways; a test asserts they lower to PTX that differs only in register
 * numbering.
 */
public class EligibleInlineLengthBound {

    public static void scaleInline(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3;
        }
    }

    public static void scaleHoisted(int[] in, int[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = in[i] * 3;
        }
    }

    /** The inline form with a `float[]`, to pin that the bound resolution
     *  is about the array parameter, not about its element type. */
    public static void scaleFloatInline(float[] in, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3f;
        }
    }
}
