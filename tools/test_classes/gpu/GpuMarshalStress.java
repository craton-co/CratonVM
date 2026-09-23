// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Adversarial cover for `vm/src/runtime/gpu_marshal.rs`.
//
// The marshaller has SIX element types (int/long/float/double/short/byte)
// and TWO paths for each: a zero-copy one that hands the JVM heap arena
// straight to the DMA, and a staged one through `host_view_*` +
// `write_back_*` for arrays with no flat base pointer. Which runs is
// decided by `zerocopy_enabled()` and by whether `heap.array_data_ptr`
// returns a pointer.
//
// `CRATONVM_GPU_NO_ZEROCOPY` switches between them, which makes this a
// built-in A/B: the two paths must agree byte for byte, and if they do
// not one of them is wrong. That is the fourth arm the runner uses.
//
// `short[]` and `byte[]` are the least-travelled: `ci-gate.sh` exercises
// `int[]` and `float[]` only, and the sign-extension rules
// (`baload` sign-extends, `bastore` truncates, `caload` zero-extends)
// are exactly where a reinterpreting `from_raw_parts` and a widening
// `host_view_*` can disagree.
//
// Sizes are deliberately not round: an odd length is what catches a
// stride or an alignment assumption, and every one is above
// `--gpu-min-work` so the dispatch is really offloaded.
//
// Usage: java GpuMarshalStress [scenario] [n]
//   0 all  1 int  2 long  3 float  4 double  5 short  6 byte  7 odd-lengths
public class GpuMarshalStress {

    // ── one kernel per element type ──────────────────────────────────

    static void scaleI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3 - 7;
        }
    }

    static void scaleJ(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3L - 7L;
        }
    }

    static void scaleF(float[] in, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3.0f - 7.0f;
        }
    }

    static void scaleD(double[] in, double[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = in[i] * 3.0 - 7.0;
        }
    }

    // `sastore` truncates to 16 bits and `saload` sign-extends; the
    // narrowing is the point, so the multiply is chosen to overflow.
    static void scaleS(short[] in, short[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (short) (in[i] * 3 - 7);
        }
    }

    // Same for 8 bits. `baload` sign-extends, so a negative input must
    // survive the round trip as a negative.
    static void scaleB(byte[] in, byte[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (byte) (in[i] * 3 - 7);
        }
    }

    static long mix(long h, long v) {
        return h * 1000003L + v;
    }

    // Raw bits for the float types: `-0.0 == 0.0` and `NaN != NaN` would
    // hide the cases worth probing.
    static long sumI(int[] v) {
        long h = 0;
        for (int x : v) h = mix(h, x);
        return h;
    }

    static long sumJ(long[] v) {
        long h = 0;
        for (long x : v) h = mix(h, x);
        return h;
    }

    static long sumF(float[] v) {
        long h = 0;
        for (float x : v) h = mix(h, Float.isNaN(x) ? 0x7fc00000 : Float.floatToRawIntBits(x));
        return h;
    }

    static long sumD(double[] v) {
        long h = 0;
        for (double x : v) {
            h = mix(h, Double.isNaN(x) ? 0x7ff8000000000000L : Double.doubleToRawLongBits(x));
        }
        return h;
    }

    static long sumS(short[] v) {
        long h = 0;
        for (short x : v) h = mix(h, x);
        return h;
    }

    static long sumB(byte[] v) {
        long h = 0;
        for (byte x : v) h = mix(h, x);
        return h;
    }

    // ── inputs that span each type's full range ──────────────────────

    static int[] mkI(int n) {
        int[] v = new int[n];
        int[] edge = {
            Integer.MIN_VALUE, Integer.MIN_VALUE + 1, -65536, -1, 0, 1,
            65535, Integer.MAX_VALUE - 1, Integer.MAX_VALUE,
        };
        for (int i = 0; i < n; i++) v[i] = (i % 17 == 0) ? edge[i % edge.length] : (i % 30011) - 15000;
        return v;
    }

    static long[] mkJ(int n) {
        long[] v = new long[n];
        long[] edge = {
            Long.MIN_VALUE, Long.MIN_VALUE + 1, -4294967296L, -1, 0, 1,
            4294967295L, Long.MAX_VALUE - 1, Long.MAX_VALUE,
        };
        for (int i = 0; i < n; i++) {
            v[i] = (i % 19 == 0) ? edge[i % edge.length] : ((long) i * 2654435761L) % 1000003L;
        }
        return v;
    }

    static float[] mkF(int n) {
        float[] v = new float[n];
        float[] edge = {
            0.0f, -0.0f, Float.MIN_VALUE, -Float.MIN_VALUE, Float.MIN_NORMAL,
            Float.MAX_VALUE, -Float.MAX_VALUE, Float.POSITIVE_INFINITY,
            Float.NEGATIVE_INFINITY, Float.NaN,
        };
        for (int i = 0; i < n; i++) v[i] = (i % 13 == 0) ? edge[i % edge.length] : (i % 7919) * 0.5f - 1000f;
        return v;
    }

    static double[] mkD(int n) {
        double[] v = new double[n];
        double[] edge = {
            0.0, -0.0, Double.MIN_VALUE, -Double.MIN_VALUE, Double.MIN_NORMAL,
            Double.MAX_VALUE, -Double.MAX_VALUE, Double.POSITIVE_INFINITY,
            Double.NEGATIVE_INFINITY, Double.NaN,
        };
        for (int i = 0; i < n; i++) v[i] = (i % 13 == 0) ? edge[i % edge.length] : (i % 7919) * 0.25 - 500.0;
        return v;
    }

    // Every one of the 65536 short values appears, so sign extension has
    // nowhere to hide.
    static short[] mkS(int n) {
        short[] v = new short[n];
        for (int i = 0; i < n; i++) v[i] = (short) (i - 32768);
        return v;
    }

    // Every one of the 256 byte values, repeatedly.
    static byte[] mkB(int n) {
        byte[] v = new byte[n];
        for (int i = 0; i < n; i++) v[i] = (byte) (i - 128);
        return v;
    }

    static long runI(int n) {
        int[] in = mkI(n), out = new int[n];
        scaleI(in, out);
        return mix(sumI(out), sumI(in));
    }

    static long runJ(int n) {
        long[] in = mkJ(n), out = new long[n];
        scaleJ(in, out);
        return mix(sumJ(out), sumJ(in));
    }

    static long runF(int n) {
        float[] in = mkF(n), out = new float[n];
        scaleF(in, out);
        return mix(sumF(out), sumF(in));
    }

    static long runD(int n) {
        double[] in = mkD(n), out = new double[n];
        scaleD(in, out);
        return mix(sumD(out), sumD(in));
    }

    static long runS(int n) {
        short[] in = mkS(n), out = new short[n];
        scaleS(in, out);
        return mix(sumS(out), sumS(in));
    }

    static long runB(int n) {
        byte[] in = mkB(n), out = new byte[n];
        scaleB(in, out);
        return mix(sumB(out), sumB(in));
    }

    // ── 7. lengths that are not round ────────────────────────────────
    //
    // A stride or alignment assumption in `host_view_*` /
    // `write_back_*` / the zero-copy reinterpret shows up on a length
    // that is not a multiple of the element width, or of the launch
    // block size, or of anything at all.
    static long oddLengths() {
        long h = 0;
        int[] lens = { 65537, 65539, 70001, 99991, 131071, 131073 };
        for (int len : lens) {
            h = mix(h, runI(len));
            h = mix(h, runS(len));
            h = mix(h, runB(len));
            h = mix(h, runF(len));
        }
        return h;
    }

    public static void main(String[] args) {
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 131072;

        if (which == 0 || which == 1) System.out.println("int=" + runI(n));
        if (which == 0 || which == 2) System.out.println("long=" + runJ(n));
        if (which == 0 || which == 3) System.out.println("float=" + runF(n));
        if (which == 0 || which == 4) System.out.println("double=" + runD(n));
        if (which == 0 || which == 5) System.out.println("short=" + runS(n));
        if (which == 0 || which == 6) System.out.println("byte=" + runB(n));
        if (which == 0 || which == 7) System.out.println("odd_lengths=" + oddLengths());
        System.out.println("MARSHAL_DONE n=" + n);
    }
}
