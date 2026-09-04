// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Should `--gpu-min-work` scale its threshold with the ELEMENT WIDTH?
//
// The 2026-09-02 pricing run measured one kernel shape -- a single
// multiply-subtract per element -- and found `long[]` a consistent
// ~1.55x LOSS in cold mode (0.59-0.65 over 5 runs) while `byte[]` won
// 2.75-3.33x. The obvious reading is that `--gpu-min-work` counts
// ELEMENTS, so an 8-byte-per-element kernel is admitted on the same
// terms as a 1-byte one, and at that transfer:compute ratio the wide
// type cannot pay for its own bytes.
//
// That reading may be right and the measurement cannot support it. One
// kernel at minimal arithmetic intensity is the worst case BY
// CONSTRUCTION: it is the point where transfer dominates most, so of
// course the widest type loses there. A threshold change would apply to
// every kernel, including ones that do real work per element. The
// question is not "does long[] lose at 1 op/element" -- it does -- but
// "where is the crossover, and is it at an arithmetic intensity real
// kernels actually sit at".
//
// So this sweeps INTENSITY as well as width. Each kernel does `ops`
// fused multiply-adds per element over the same array, so the bytes
// moved are identical across intensities and only the compute changes.
// If the crossover is at 1-2 ops, a width-scaled threshold is real. If
// long[] is already winning by 8 ops, the original finding is a
// statement about one degenerate shape and the default should not move.
//
// Cold mode only: the input is mutated between calls so every submit
// pays H2D + D2H. That is the case where transfer cost is visible at
// all -- in hot mode the residency cache serves the input and the
// question does not arise.
//
// Usage: java GpuIntensitySweep <type:I|J|F|D|S|B> <n> <iters> <ops>
public class GpuIntensitySweep {

    // Each kernel is a counted loop with a fixed number of multiply-adds
    // per element. `ops` is a parameter of the GENERATOR, not of the
    // kernel: a kernel taking a loop bound would not be admitted, and an
    // inner loop over `ops` would change the shape being lowered.

    static void workI1(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) { int v = in[i]; out[i] = v * 3 - 7; }
    }

    static void workI4(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7; v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            out[i] = v;
        }
    }

    static void workI16(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7;  v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            v = v * 3 - 7;  v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            out[i] = v;
        }
    }

    static void workJ1(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) { long v = in[i]; out[i] = v * 3L - 7L; }
    }

    static void workJ4(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) {
            long v = in[i];
            v = v * 3L - 7L; v = v * 5L + 11L; v = v * 7L - 13L; v = v * 11L + 17L;
            out[i] = v;
        }
    }

    static void workJ16(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) {
            long v = in[i];
            v = v * 3L - 7L;  v = v * 5L + 11L; v = v * 7L - 13L; v = v * 11L + 17L;
            v = v * 13L - 19L; v = v * 17L + 23L; v = v * 19L - 29L; v = v * 23L + 31L;
            v = v * 3L - 7L;  v = v * 5L + 11L; v = v * 7L - 13L; v = v * 11L + 17L;
            v = v * 13L - 19L; v = v * 17L + 23L; v = v * 19L - 29L; v = v * 23L + 31L;
            out[i] = v;
        }
    }

    static void workD1(double[] in, double[] out) {
        for (int i = 0; i < out.length; i++) { double v = in[i]; out[i] = v * 3.0 - 7.0; }
    }

    static void workD4(double[] in, double[] out) {
        for (int i = 0; i < out.length; i++) {
            double v = in[i];
            v = v * 3.0 - 7.0; v = v * 5.0 + 11.0; v = v * 7.0 - 13.0; v = v * 11.0 + 17.0;
            out[i] = v;
        }
    }

    static void workD16(double[] in, double[] out) {
        for (int i = 0; i < out.length; i++) {
            double v = in[i];
            v = v * 3.0 - 7.0;  v = v * 5.0 + 11.0; v = v * 7.0 - 13.0; v = v * 11.0 + 17.0;
            v = v * 13.0 - 19.0; v = v * 17.0 + 23.0; v = v * 19.0 - 29.0; v = v * 23.0 + 31.0;
            v = v * 3.0 - 7.0;  v = v * 5.0 + 11.0; v = v * 7.0 - 13.0; v = v * 11.0 + 17.0;
            v = v * 13.0 - 19.0; v = v * 17.0 + 23.0; v = v * 19.0 - 29.0; v = v * 23.0 + 31.0;
            out[i] = v;
        }
    }

    static void workB1(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) { int v = in[i]; out[i] = (byte) (v * 3 - 7); }
    }

    static void workB4(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7; v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            out[i] = (byte) v;
        }
    }

    static void workB16(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7;  v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            v = v * 3 - 7;  v = v * 5 + 11; v = v * 7 - 13; v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            out[i] = (byte) v;
        }
    }

    static long sink;

    public static void main(String[] args) {
        String type = args.length > 0 ? args[0] : "I";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        int ops = args.length > 3 ? Integer.parseInt(args[3]) : 1;

        int[] iIn = null, iOut = null;
        long[] jIn = null, jOut = null;
        double[] dIn = null, dOut = null;
        byte[] bIn = null, bOut = null;
        switch (type) {
            case "I": iIn = new int[n];    iOut = new int[n];    break;
            case "J": jIn = new long[n];   jOut = new long[n];   break;
            case "D": dIn = new double[n]; dOut = new double[n]; break;
            case "B": bIn = new byte[n];   bOut = new byte[n];   break;
            default: throw new IllegalArgumentException(type);
        }
        for (int i = 0; i < n; i++) {
            switch (type) {
                case "I": iIn[i] = i % 1013; break;
                case "J": jIn[i] = i % 1013; break;
                case "D": dIn[i] = i % 1013; break;
                case "B": bIn[i] = (byte) (i % 113); break;
            }
        }

        for (int w = 0; w < 20; w++) run(type, ops, iIn, iOut, jIn, jOut, dIn, dOut, bIn, bOut);

        long t0 = System.nanoTime();
        for (int it = 0; it < iters; it++) {
            // Cold: evict the residency entry so every submit pays H2D.
            switch (type) {
                case "I": iIn[it % n] += 1; break;
                case "J": jIn[it % n] += 1; break;
                case "D": dIn[it % n] += 1; break;
                case "B": bIn[it % n] += 1; break;
            }
            run(type, ops, iIn, iOut, jIn, jOut, dIn, dOut, bIn, bOut);
        }
        long ns = System.nanoTime() - t0;

        switch (type) {
            case "I": sink = iOut[0] + iOut[n - 1]; break;
            case "J": sink = jOut[0] + jOut[n - 1]; break;
            case "D": sink = (long) (dOut[0] + dOut[n - 1]); break;
            case "B": sink = bOut[0] + bOut[n - 1]; break;
        }
        System.out.println("type=" + type + " n=" + n + " iters=" + iters + " ops=" + ops
                + " us_per_call=" + (ns / 1000L / iters) + " sink=" + sink);
    }

    static void run(String type, int ops, int[] iIn, int[] iOut, long[] jIn, long[] jOut,
                    double[] dIn, double[] dOut, byte[] bIn, byte[] bOut) {
        switch (type) {
            case "I":
                if (ops == 1) workI1(iIn, iOut);
                else if (ops == 4) workI4(iIn, iOut);
                else workI16(iIn, iOut);
                break;
            case "J":
                if (ops == 1) workJ1(jIn, jOut);
                else if (ops == 4) workJ4(jIn, jOut);
                else workJ16(jIn, jOut);
                break;
            case "D":
                if (ops == 1) workD1(dIn, dOut);
                else if (ops == 4) workD4(dIn, dOut);
                else workD16(dIn, dOut);
                break;
            case "B":
                if (ops == 1) workB1(bIn, bOut);
                else if (ops == 4) workB4(bIn, bOut);
                else workB16(bIn, bOut);
                break;
        }
    }
}
