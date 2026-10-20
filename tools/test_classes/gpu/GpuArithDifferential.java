// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Differential oracle for the opcodes `jit-cuda` lowers BY HAND.
//
// The emitter claims bit-exactness for a long list of these — the shift
// masks (JLS 15.19 vs PTX's clamp), the integer division guards, the
// narrowing conversions, the float→int saturation. Every one of those
// claims is currently backed by a comment. This runs them.
//
// Shape rules, so every kernel below is actually offloaded rather than
// silently falling back to the interpreter and comparing HotSpot with
// HotSpot:
//
//   * `static`, `void`, primitive arrays only;
//   * one canonical counted loop `for (i = 0; i < out.length; i++)`;
//   * no calls, no allocation, no field access, no `switch`, no `throw`;
//   * no `frem`/`drem` (rejected under the default `Strict` hint) and no
//     `lcmp`/`fcmp*` (admitted by the analyzer, then refused by the
//     emitter at the `if*` that consumes them).
//
// Results are compared as RAW BITS, never as `==`: `-0.0 == 0.0` is true
// and `NaN == NaN` is false, so a value comparison would hide exactly the
// cases these inputs exist to probe.
//
// Usage: java GpuArithDifferential [n]
public class GpuArithDifferential {

    // ── integer division and remainder ───────────────────────────────
    // Java truncates toward zero and defines MIN_VALUE / -1 as
    // MIN_VALUE (JLS 15.17.2). A zero divisor must reach the deopt
    // flag, not produce a device-defined value.
    static void idivKernel(int[] a, int[] b, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] / b[i];
        }
    }

    static void iremKernel(int[] a, int[] b, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] % b[i];
        }
    }

    static void ldivKernel(long[] a, long[] b, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] / b[i];
        }
    }

    static void lremKernel(long[] a, long[] b, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] % b[i];
        }
    }

    // ── shifts: JLS 15.19 masks the count, PTX clamps it ─────────────
    // `1 << 32` is 1 in Java and 0 under a clamping shift. The counts
    // below deliberately run past the operand width and negative.
    static void ishlKernel(int[] a, int[] n, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] << n[i];
        }
    }

    static void ishrKernel(int[] a, int[] n, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] >> n[i];
        }
    }

    static void iushrKernel(int[] a, int[] n, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] >>> n[i];
        }
    }

    static void lshlKernel(long[] a, int[] n, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] << n[i];
        }
    }

    static void lushrKernel(long[] a, int[] n, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] >>> n[i];
        }
    }

    // ── narrowing conversions ────────────────────────────────────────
    static void i2bKernel(int[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (byte) a[i];
        }
    }

    static void i2cKernel(int[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (char) a[i];
        }
    }

    static void i2sKernel(int[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (short) a[i];
        }
    }

    // ── float→int: Java SATURATES, NaN becomes 0 (JLS 5.1.3) ─────────
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

    static void d2iKernel(double[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (int) a[i];
        }
    }

    static void l2iKernel(long[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (int) a[i];
        }
    }

    // ── float arithmetic: -0.0, NaN payloads, inf, subnormals ────────
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

    static void ddivKernel(double[] a, double[] b, double[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i] / b[i];
        }
    }

    static void f2dKernel(float[] a, double[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i];
        }
    }

    static void d2fKernel(double[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = (float) a[i];
        }
    }

    static void i2fKernel(int[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i];
        }
    }

    static void l2fKernel(long[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = a[i];
        }
    }

    // ── negation at the boundary: -MIN_VALUE is MIN_VALUE ────────────
    static void inegKernel(int[] a, int[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = -a[i];
        }
    }

    static void lnegKernel(long[] a, long[] out) {
        for (int i = 0; i < out.length; i++) {
            out[i] = -a[i];
        }
    }

    // ─────────────────────────────────────────────────────────────────

    static final int[] INT_EDGE = {
        Integer.MIN_VALUE, Integer.MIN_VALUE + 1, -2147483647, -65536, -32769, -32768,
        -256, -129, -128, -127, -2, -1, 0, 1, 2, 126, 127, 128, 254, 255, 256,
        32767, 32768, 65535, 65536, 2147483646, Integer.MAX_VALUE,
    };

    static final long[] LONG_EDGE = {
        Long.MIN_VALUE, Long.MIN_VALUE + 1, -4294967296L, -2147483649L,
        (long) Integer.MIN_VALUE, -65536, -1, 0, 1, 65535, Integer.MAX_VALUE,
        2147483648L, 4294967295L, 4294967296L, Long.MAX_VALUE - 1, Long.MAX_VALUE,
    };

    // Shift counts that run past the operand width and go negative —
    // the whole point of the mask.
    static final int[] SHIFT_EDGE = {
        -64, -33, -32, -1, 0, 1, 7, 30, 31, 32, 33, 62, 63, 64, 65, 95, 96, 127,
    };

    static int[] intInputs(int n) {
        int[] v = new int[n];
        for (int i = 0; i < n; i++) v[i] = INT_EDGE[i % INT_EDGE.length];
        return v;
    }

    static int[] intDivisors(int n) {
        int[] v = new int[n];
        // Rotated so MIN_VALUE / -1 occurs, which is the edge case Java
        // defines (JLS 15.17.2) and a naive `div.s32` gets wrong.
        //
        // Zero is mapped away deliberately: a zero divisor THROWS in
        // Java, so it cannot be part of a value comparison. The deopt
        // path it drives on the device is a different question with its
        // own fixture (`BoundsDeopt*`), and conflating the two would
        // mean this whole file could only ever report "it threw".
        for (int i = 0; i < n; i++) {
            int d = INT_EDGE[(i * 7 + 11) % INT_EDGE.length];
            v[i] = d == 0 ? 1 : d;
        }
        return v;
    }

    static long[] longInputs(int n) {
        long[] v = new long[n];
        for (int i = 0; i < n; i++) v[i] = LONG_EDGE[i % LONG_EDGE.length];
        return v;
    }

    static long[] longDivisors(int n) {
        long[] v = new long[n];
        // See `intDivisors` for why zero is mapped away.
        for (int i = 0; i < n; i++) {
            long d = LONG_EDGE[(i * 5 + 3) % LONG_EDGE.length];
            v[i] = d == 0 ? 1 : d;
        }
        return v;
    }

    static int[] shiftCounts(int n) {
        int[] v = new int[n];
        for (int i = 0; i < n; i++) v[i] = SHIFT_EDGE[i % SHIFT_EDGE.length];
        return v;
    }

    static float[] floatInputs(int n) {
        // NaN payloads are built by hand: Float.NaN is one bit pattern,
        // and a lowering that canonicalises NaN would pass a test that
        // only ever fed it that one.
        float[] special = {
            0.0f, -0.0f, 1.0f, -1.0f, Float.MIN_VALUE, -Float.MIN_VALUE,
            Float.MIN_NORMAL, Float.MAX_VALUE, -Float.MAX_VALUE,
            Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY,
            Float.intBitsToFloat(0x7fc00000), Float.intBitsToFloat(0x7f800001),
            Float.intBitsToFloat(0xffc0dead), Float.intBitsToFloat(0x7fabcdef),
            3.4028235e38f, 1.4e-45f, 2.5f, -2.5f, 0.5f,
            2147483520f, 2147483904f, -2147483648f, -2147483904f,
            9.223372e18f, -9.223372e18f, 1e30f, -1e30f,
        };
        float[] v = new float[n];
        for (int i = 0; i < n; i++) v[i] = special[i % special.length];
        return v;
    }

    static float[] floatDivisors(int n) {
        float[] src = floatInputs(n);
        float[] v = new float[n];
        for (int i = 0; i < n; i++) v[i] = src[(i * 13 + 5) % n];
        return v;
    }

    static double[] doubleInputs(int n) {
        double[] special = {
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
        double[] v = new double[n];
        for (int i = 0; i < n; i++) v[i] = special[i % special.length];
        return v;
    }

    static double[] doubleDivisors(int n) {
        double[] src = doubleInputs(n);
        double[] v = new double[n];
        for (int i = 0; i < n; i++) v[i] = src[(i * 11 + 7) % n];
        return v;
    }

    static long mix(long h, long v) {
        // A checksum that does not commute, so a permutation of the same
        // values is a different answer.
        return h * 1000003L + v;
    }

    static long sumInts(int[] v) {
        long h = 0;
        for (int x : v) h = mix(h, x);
        return h;
    }

    static long sumLongs(long[] v) {
        long h = 0;
        for (long x : v) h = mix(h, x);
        return h;
    }

    // NaN payloads are collapsed to ONE value before hashing.
    //
    // JLS 4.2.3 does not specify which NaN bit pattern an arithmetic
    // operation produces and the PTX ISA says outright that "NaN inputs
    // yield an unspecified NaN". The device canonicalises to 0x7fffffff
    // where HotSpot propagates, so hashing raw payloads reports a
    // difference that is real, permanent, and not a defect — see
    // docs/gpu/lowering-architecture.md.
    //
    // Everything else stays raw: -0.0 and +0.0 hash differently, which
    // is the point of using raw bits at all.
    static final int CANON_F = 0x7fc00000;
    static final long CANON_D = 0x7ff8000000000000L;

    static long sumFloats(float[] v) {
        long h = 0;
        for (float x : v) {
            h = mix(h, Float.isNaN(x) ? CANON_F : Float.floatToRawIntBits(x));
        }
        return h;
    }

    static long sumDoubles(double[] v) {
        long h = 0;
        for (double x : v) {
            h = mix(h, Double.isNaN(x) ? CANON_D : Double.doubleToRawLongBits(x));
        }
        return h;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4096;

        int[] ia = intInputs(n), ib = intDivisors(n), sh = shiftCounts(n);
        long[] la = longInputs(n), lb = longDivisors(n);
        float[] fa = floatInputs(n), fb = floatDivisors(n);
        double[] da = doubleInputs(n), db = doubleDivisors(n);

        int[] io = new int[n];
        long[] lo = new long[n];
        float[] fo = new float[n];
        double[] doub = new double[n];

        idivKernel(ia, ib, io);   System.out.println("idiv=" + sumInts(io));
        iremKernel(ia, ib, io);   System.out.println("irem=" + sumInts(io));
        ldivKernel(la, lb, lo);   System.out.println("ldiv=" + sumLongs(lo));
        lremKernel(la, lb, lo);   System.out.println("lrem=" + sumLongs(lo));

        ishlKernel(ia, sh, io);   System.out.println("ishl=" + sumInts(io));
        ishrKernel(ia, sh, io);   System.out.println("ishr=" + sumInts(io));
        iushrKernel(ia, sh, io);  System.out.println("iushr=" + sumInts(io));
        lshlKernel(la, sh, lo);   System.out.println("lshl=" + sumLongs(lo));
        lushrKernel(la, sh, lo);  System.out.println("lushr=" + sumLongs(lo));

        i2bKernel(ia, io);        System.out.println("i2b=" + sumInts(io));
        i2cKernel(ia, io);        System.out.println("i2c=" + sumInts(io));
        i2sKernel(ia, io);        System.out.println("i2s=" + sumInts(io));

        f2iKernel(fa, io);        System.out.println("f2i=" + sumInts(io));
        f2lKernel(fa, lo);        System.out.println("f2l=" + sumLongs(lo));
        d2iKernel(da, io);        System.out.println("d2i=" + sumInts(io));
        l2iKernel(la, io);        System.out.println("l2i=" + sumInts(io));

        faddKernel(fa, fb, fo);   System.out.println("fadd=" + sumFloats(fo));
        fmulKernel(fa, fb, fo);   System.out.println("fmul=" + sumFloats(fo));
        fdivKernel(fa, fb, fo);   System.out.println("fdiv=" + sumFloats(fo));
        fnegKernel(fa, fo);       System.out.println("fneg=" + sumFloats(fo));
        ddivKernel(da, db, doub); System.out.println("ddiv=" + sumDoubles(doub));

        f2dKernel(fa, doub);      System.out.println("f2d=" + sumDoubles(doub));
        d2fKernel(da, fo);        System.out.println("d2f=" + sumFloats(fo));
        i2fKernel(ia, fo);        System.out.println("i2f=" + sumFloats(fo));
        l2fKernel(la, fo);        System.out.println("l2f=" + sumFloats(fo));

        inegKernel(ia, io);       System.out.println("ineg=" + sumInts(io));
        lnegKernel(la, lo);       System.out.println("lneg=" + sumLongs(lo));

        System.out.println("ARITH_DONE n=" + n);
    }
}
