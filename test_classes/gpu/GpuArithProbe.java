// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Per-element dump for the kernels `GpuArithDifferential` reported as
// divergent. A checksum says THAT two runs disagree; this says on which
// input, which is the only form the answer can be acted on in.
//
// Prints one line per element as raw bits, so a diff of two runs points
// straight at the operand pair. Same shape rules as
// `GpuArithDifferential` so the kernels are really offloaded.
//
// Usage: java GpuArithProbe <kernel> [n]
public class GpuArithProbe {

    static void faddKernel(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] + b[i];
        }
    }

    static void fmulKernel(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] * b[i];
        }
    }

    static void fdivKernel(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] / b[i];
        }
    }

    static void fnegKernel(float[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = -a[i];
        }
    }

    static void f2iKernel(float[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (int) a[i];
        }
    }

    static void f2lKernel(float[] a, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (long) a[i];
        }
    }

    static void d2lKernel(double[] a, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (long) a[i];
        }
    }

    static void d2iKernel(double[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (int) a[i];
        }
    }

    static final float[] F = {
        0.0f, -0.0f, 1.0f, -1.0f, Float.MIN_VALUE, -Float.MIN_VALUE,
        Float.MIN_NORMAL, Float.MAX_VALUE, -Float.MAX_VALUE,
        Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY,
        Float.intBitsToFloat(0x7fc00000), Float.intBitsToFloat(0x7f800001),
        Float.intBitsToFloat(0xffc0dead), Float.intBitsToFloat(0x7fabcdef),
        3.4028235e38f, 1.4e-45f, 2.5f, -2.5f, 0.5f,
        2147483520f, 2147483904f, -2147483648f, -2147483904f,
        9.223372e18f, -9.223372e18f, 1e30f, -1e30f,
    };

    static final double[] D = {
        0.0, -0.0, 1.0, -1.0, Double.MIN_VALUE, -Double.MIN_VALUE,
        Double.MIN_NORMAL, Double.MAX_VALUE, -Double.MAX_VALUE,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY,
        Double.longBitsToDouble(0x7ff8000000000000L),
        Double.longBitsToDouble(0x7ff0000000000001L),
        Double.longBitsToDouble(0xfff8deadbeefcafeL),
        2.5, -2.5, 0.5, 2147483647.5, -2147483649.0,
        9.223372036854776e18, -9.223372036854776e18, 1e300, -1e300,
        3.4028236e38, -3.4028236e38, 1e-320,
    };

    /// Raw bits, or a single token when the value is NaN and payloads
    /// are being ignored. See `canon` in `main`.
    static String f(float v, boolean canon) {
        int bits = Float.floatToRawIntBits(v);
        if (canon && Float.isNaN(v)) return "NaN";
        return Integer.toHexString(bits);
    }

    static String d(double v, boolean canon) {
        long bits = Double.doubleToRawLongBits(v);
        if (canon && Double.isNaN(v)) return "NaN";
        return Long.toHexString(bits);
    }

    public static void main(String[] args) {
        // Dispatch on an INT code, never on String.equals.
        // A harness must not depend on machinery that is itself under
        // test — and `String.equals` is currently wrong under the JIT
        // (see JitStringBranch.java), which made an earlier version of
        // this file report the GPU as diverging when it was the host
        // picking the wrong arm.
        //   0 fadd  1 fmul  2 fdiv  3 fneg  4 d2i  5 f2i  6 f2l  7 d2l
        int which = args.length > 0 ? Integer.parseInt(args[0]) : 0;
        // args[2]=1 collapses every NaN result to one token before
        // printing. NaN PAYLOADS are unspecified by both JLS 4.2.3 and
        // the PTX ISA, and the device does not preserve them, so a raw
        // comparison reports a difference that is real, permanent, and
        // not a defect — which would make this unusable as a gate.
        // Raw mode (the default) stays for investigating one.
        boolean canon = args.length > 2 && args[2].charAt(0) == '1';
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 256;

        float[] fa = new float[n], fb = new float[n], fo = new float[n];
        double[] da = new double[n];
        int[] io = new int[n];
        long[] lo = new long[n];
        for (int i = 0; i < n; i++) {
            fa[i] = F[i % F.length];
            fb[i] = F[(i * 13 + 5) % F.length];
            da[i] = D[i % D.length];
        }

        switch (which) {
            case 0: faddKernel(fa, fb, fo); break;
            case 1: fmulKernel(fa, fb, fo); break;
            case 2: fdivKernel(fa, fb, fo); break;
            case 3: fnegKernel(fa, fo); break;
            case 4: d2iKernel(da, io); break;
            case 5: f2iKernel(fa, io); break;
            case 6: f2lKernel(fa, lo); break;
            case 7: d2lKernel(da, lo); break;
            default: System.out.println("unknown kernel " + which); return;
        }

        for (int i = 0; i < n; i++) {
            switch (which) {
                case 4:
                    System.out.println(i + " in="
                            + d(da[i], canon) + " out=" + io[i]);
                    break;
                case 5:
                    System.out.println(i + " in="
                            + f(fa[i], canon) + " out=" + io[i]);
                    break;
                case 6:
                    System.out.println(i + " in="
                            + f(fa[i], canon) + " out=" + lo[i]);
                    break;
                case 7:
                    System.out.println(i + " in="
                            + d(da[i], canon) + " out=" + lo[i]);
                    break;
                case 3:
                    System.out.println(i + " a="
                            + f(fa[i], canon) + " out=" + f(fo[i], canon));
                    break;
                default:
                    System.out.println(i + " a="
                            + f(fa[i], canon) + " b=" + f(fb[i], canon)
                            + " out=" + f(fo[i], canon));
                    break;
            }
        }
        System.out.println("PROBE_DONE " + which + " n=" + n);
    }
}
