// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Does offloading a NARROW element type actually pay?
//
// `short[]` and `byte[]` became offloadable on 2026-09-02 (before that
// the marshaller refused them and every such kernel silently re-ran on
// the interpreter). Correctness was proved; the cost was not, and the
// arithmetic intensity here is poor by construction:
//
//   scaleS moves 2 bytes in and 2 bytes out per element and does one
//   multiply-subtract on it. scaleB moves 1 byte each way for the same
//   single operation. `--gpu-min-work` counts ELEMENTS, not bytes, so
//   the same 4096 threshold admits an int[] kernel moving 32 KB and a
//   byte[] kernel moving 8 KB -- a quarter of the transfer for the same
//   arithmetic.
//
// If a narrow kernel is slower on the device than in the interpreter,
// the silent fallback that existed until 2026-09-02 was accidentally
// doing the right thing, and the threshold wants to scale with the
// element width rather than the element count.
//
// TWO MODES, because they measure different things:
//
//   hot    the same input array is submitted `iters` times unchanged.
//          The input-residency cache holds the device buffer, so only
//          the first submit pays H2D. This is the best case for the
//          GPU and the one a naive benchmark reports.
//   cold   the input is mutated between submits, so the cache is
//          evicted and EVERY submit pays H2D + D2H. This is what a
//          workload with changing data actually costs.
//
// A verdict from `hot` alone would be dishonest: it prices a transfer
// that the measured workload does not perform.
//
// Usage: java GpuWidthPricing <type:I|J|F|D|S|B> <n> <iters> <hot|cold>
public class GpuWidthPricing {

    // The same six kernels GpuMarshalStress uses, so the shapes being
    // priced are the shapes the correctness gate covers.

    static void scaleI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3 - 7;
    }

    static void scaleJ(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3L - 7L;
    }

    static void scaleF(float[] in, float[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3.0f - 7.0f;
    }

    static void scaleD(double[] in, double[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3.0 - 7.0;
    }

    static void scaleS(short[] in, short[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (short) (in[i] * 3 - 7);
    }

    static void scaleB(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) out[i] = (byte) (in[i] * 3 - 7);
    }

    // Consume the output so nothing can be optimised away, and give a
    // value that differs if the arm computed the wrong thing.
    static long sink;

    public static void main(String[] args) {
        String type = args.length > 0 ? args[0] : "I";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        boolean cold = args.length > 3 && args[3].equals("cold");

        int[] iIn = null, iOut = null;
        long[] jIn = null, jOut = null;
        float[] fIn = null, fOut = null;
        double[] dIn = null, dOut = null;
        short[] sIn = null, sOut = null;
        byte[] bIn = null, bOut = null;

        switch (type) {
            case "I": iIn = new int[n];    iOut = new int[n];    break;
            case "J": jIn = new long[n];   jOut = new long[n];   break;
            case "F": fIn = new float[n];  fOut = new float[n];  break;
            case "D": dIn = new double[n]; dOut = new double[n]; break;
            case "S": sIn = new short[n];  sOut = new short[n];  break;
            case "B": bIn = new byte[n];   bOut = new byte[n];   break;
            default: throw new IllegalArgumentException(type);
        }
        for (int i = 0; i < n; i++) {
            switch (type) {
                case "I": iIn[i] = i % 1013; break;
                case "J": jIn[i] = i % 1013; break;
                case "F": fIn[i] = i % 1013; break;
                case "D": dIn[i] = i % 1013; break;
                case "S": sIn[i] = (short) (i % 1013); break;
                case "B": bIn[i] = (byte) (i % 113); break;
            }
        }

        // Warm: enough calls to clear the offload admission counter and
        // any JIT threshold, so the timed region measures steady state
        // rather than compilation.
        for (int w = 0; w < 20; w++) run(type, iIn, iOut, jIn, jOut, fIn, fOut, dIn, dOut, sIn, sOut, bIn, bOut);

        long t0 = System.nanoTime();
        for (int it = 0; it < iters; it++) {
            if (cold) {
                // One host write is enough to evict the residency entry
                // for this array, so the next submit re-uploads.
                switch (type) {
                    case "I": iIn[it % n] += 1; break;
                    case "J": jIn[it % n] += 1; break;
                    case "F": fIn[it % n] += 1; break;
                    case "D": dIn[it % n] += 1; break;
                    case "S": sIn[it % n] += 1; break;
                    case "B": bIn[it % n] += 1; break;
                }
            }
            run(type, iIn, iOut, jIn, jOut, fIn, fOut, dIn, dOut, sIn, sOut, bIn, bOut);
        }
        long ns = System.nanoTime() - t0;

        switch (type) {
            case "I": sink = iOut[0] + iOut[n - 1]; break;
            case "J": sink = jOut[0] + jOut[n - 1]; break;
            case "F": sink = (long) (fOut[0] + fOut[n - 1]); break;
            case "D": sink = (long) (dOut[0] + dOut[n - 1]); break;
            case "S": sink = sOut[0] + sOut[n - 1]; break;
            case "B": sink = bOut[0] + bOut[n - 1]; break;
        }

        System.out.println("type=" + type + " n=" + n + " iters=" + iters
                + " mode=" + (cold ? "cold" : "hot")
                + " total_ms=" + (ns / 1_000_000L)
                + " us_per_call=" + (ns / 1000L / iters)
                + " sink=" + sink);
    }

    static void run(String type, int[] iIn, int[] iOut, long[] jIn, long[] jOut,
                    float[] fIn, float[] fOut, double[] dIn, double[] dOut,
                    short[] sIn, short[] sOut, byte[] bIn, byte[] bOut) {
        switch (type) {
            case "I": scaleI(iIn, iOut); break;
            case "J": scaleJ(jIn, jOut); break;
            case "F": scaleF(fIn, fOut); break;
            case "D": scaleD(dIn, dOut); break;
            case "S": scaleS(sIn, sOut); break;
            case "B": scaleB(bIn, bOut); break;
        }
    }
}
