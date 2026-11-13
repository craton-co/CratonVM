// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 2, lane INTRINS: facts for the optimizing tier's pure call-site
 * expansions (jit/src/ir/pure_expand.rs) and for the already-covered
 * exception-raising intrinsic rows they sit next to.
 *
 *   CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND  Integer.reverse / Long.reverse
 *   CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND    Math.copySign (D and F)
 *   CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND      Math / StrictMath.signum (D and F)
 *   CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND      Double / Float isNaN / isInfinite / isFinite
 *                                           (j27 wave 6, lane T1BCE)
 *   CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND     Integer / Long compress / expand (BMI2 PEXT /
 *                                           PDEP; OPT-IN, BMI2 hosts only; j27 wave 6)
 *
 * The first four default ON, the last default OFF (run it =1 as well); every stdout line must equal HotSpot 25 byte for byte
 * in both modes, unset and with each switch =0. The "*-mismatches" facts check
 * every answer of a warmed (compiled) caller against an independent Java
 * reference; the literal facts are listed in
 * docs/known-issues/jit/j27-intrinsic-coverage-vs-hotspot-20261028.md.
 *
 * The exception rows warm a non-throwing caller first, so the throwing call
 * runs compiled code (the IR splice of the Math.*Exact bodies deopts into the
 * callee frame; Preconditions.checkIndex is a thin helper whose decline
 * dispatches the JDK body): class and message must be HotSpot's.
 *
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=1 (or =true).
 */
public final class J27IntrinsicRows {
    static final int WARM = 60_000;

    static int rev(int x) {
        return Integer.reverse(x);
    }

    static long rev(long x) {
        return Long.reverse(x);
    }

    static double copySign(double m, double s) {
        return Math.copySign(m, s);
    }

    static float copySign(float m, float s) {
        return Math.copySign(m, s);
    }

    static double strictCopySign(double m, double s) {
        return StrictMath.copySign(m, s);
    }

    static double signum(double d) {
        return Math.signum(d);
    }

    static float signum(float f) {
        return Math.signum(f);
    }

    static double strictSignum(double d) {
        return StrictMath.signum(d);
    }

    static boolean isNaN(double d) {
        return Double.isNaN(d);
    }

    static boolean isInfinite(double d) {
        return Double.isInfinite(d);
    }

    static boolean isFinite(double d) {
        return Double.isFinite(d);
    }

    static boolean isNaN(float f) {
        return Float.isNaN(f);
    }

    static boolean isInfinite(float f) {
        return Float.isInfinite(f);
    }

    static boolean isFinite(float f) {
        return Float.isFinite(f);
    }

    /** "nif" flags of the raw bits, from the IEEE layout alone (no FP compare). */
    static String refClass(long b) {
        long abs = b & Long.MAX_VALUE;
        long inf = 0x7ff0000000000000L;
        return (abs > inf ? "1" : "0") + (abs == inf ? "1" : "0") + (abs < inf ? "1" : "0");
    }

    static String refClass(int b) {
        int abs = b & Integer.MAX_VALUE;
        int inf = 0x7f800000;
        return (abs > inf ? "1" : "0") + (abs == inf ? "1" : "0") + (abs < inf ? "1" : "0");
    }

    static String flags(boolean nan, boolean inf, boolean fin) {
        return (nan ? "1" : "0") + (inf ? "1" : "0") + (fin ? "1" : "0");
    }

    static String classify(double d) {
        return flags(isNaN(d), isInfinite(d), isFinite(d));
    }

    static String classify(float f) {
        return flags(isNaN(f), isInfinite(f), isFinite(f));
    }

    static int classifyMismatches() {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            long b = (r & 1) == 0 ? mix(r) : DOUBLE_BITS[r % DOUBLE_BITS.length];
            if (!classify(Double.longBitsToDouble(b)).equals(refClass(b))) {
                bad++;
            }
            int fb = (r & 1) == 0 ? (int) mix(r) : FLOAT_BITS[r % FLOAT_BITS.length];
            if (!classify(Float.intBitsToFloat(fb)).equals(refClass(fb))) {
                bad++;
            }
        }
        return bad;
    }

    static int compressI(int i, int mask) {
        return Integer.compress(i, mask);
    }

    static int expandI(int i, int mask) {
        return Integer.expand(i, mask);
    }

    static long compressL(long i, long mask) {
        return Long.compress(i, mask);
    }

    static long expandL(long i, long mask) {
        return Long.expand(i, mask);
    }

    /** The javadoc's definition, bit by bit (not the JDK's own algorithm). */
    static long refCompress(long x, long mask, int width) {
        long r = 0;
        int k = 0;
        for (int b = 0; b < width; b++) {
            if ((mask >>> b & 1) != 0) {
                r |= (x >>> b & 1) << k++;
            }
        }
        return r;
    }

    static long refExpand(long x, long mask, int width) {
        long r = 0;
        int k = 0;
        for (int b = 0; b < width; b++) {
            if ((mask >>> b & 1) != 0) {
                r |= (x >>> k++ & 1) << b;
            }
        }
        return r;
    }

    static int compressMismatches() {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            long x = mix(r);
            long m = (r & 3) == 0 ? -1L : (r & 3) == 1 ? 0xAAAAAAAAAAAAAAAAL : mix(r + 7);
            int xi = (int) x;
            int mi = (int) m;
            if (compressI(xi, mi) != (int) refCompress(xi & 0xFFFFFFFFL, mi & 0xFFFFFFFFL, 32)) {
                bad++;
            }
            if (expandI(xi, mi) != (int) refExpand(xi & 0xFFFFFFFFL, mi & 0xFFFFFFFFL, 32)) {
                bad++;
            }
            if (compressL(x, m) != refCompress(x, m, 64)) {
                bad++;
            }
            if (expandL(x, m) != refExpand(x, m, 64)) {
                bad++;
            }
        }
        return bad;
    }

    static int refRev(int x) {
        int r = 0;
        for (int i = 0; i < 32; i++) {
            r |= ((x >>> i) & 1) << (31 - i);
        }
        return r;
    }

    static long refRev(long x) {
        long r = 0;
        for (int i = 0; i < 64; i++) {
            r |= ((x >>> i) & 1L) << (63 - i);
        }
        return r;
    }

    static long refCopySign(long m, long s) {
        return s < 0 ? m | Long.MIN_VALUE : m & Long.MAX_VALUE;
    }

    static int refCopySign(int m, int s) {
        return s < 0 ? m | Integer.MIN_VALUE : m & Integer.MAX_VALUE;
    }

    static long refSignum(long b) {
        double d = Double.longBitsToDouble(b);
        if (d == 0.0 || d != d) {
            return b;
        }
        return Double.doubleToRawLongBits(d > 0 ? 1.0 : -1.0);
    }

    static int refSignum(int b) {
        float f = Float.intBitsToFloat(b);
        if (f == 0.0f || f != f) {
            return b;
        }
        return Float.floatToRawIntBits(f > 0 ? 1.0f : -1.0f);
    }

    /** Quiet NaNs only: a signalling NaN's bits are not portable through a call. */
    static final long[] DOUBLE_BITS = {
        0x0L, 0x8000000000000000L, 0x3ff0000000000000L, 0xc000000000000000L,
        0x7ff0000000000000L, 0xfff0000000000000L, 0x7ff8000000000000L,
        0xfff8000000000123L, 0x7ff8000000000456L, 0x1L, 0x8000000000000001L,
        0x7fefffffffffffffL, 0x4045000000000000L, 0xbfe0000000000000L,
    };

    static final int[] FLOAT_BITS = {
        0x0, 0x80000000, 0x3f800000, 0xc0000000, 0x7f800000, 0xff800000,
        0x7fc00000, 0xffc00123, 0x7fc00456, 0x1, 0x80000001, 0x7f7fffff,
        0x42280000, 0xbf000000,
    };

    static long mix(long i) {
        return i * 0x9E3779B97F4A7C15L ^ (i >>> 7);
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String hex(double d) {
        return Long.toHexString(Double.doubleToRawLongBits(d));
    }

    static String hex(float f) {
        return Integer.toHexString(Float.floatToRawIntBits(f));
    }

    static int reverseMismatches() {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            long v = mix(r);
            if (rev((int) v) != refRev((int) v)) {
                bad++;
            }
            if (rev(v) != refRev(v)) {
                bad++;
            }
        }
        return bad;
    }

    static int copySignMismatches() {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            long m = DOUBLE_BITS[r % DOUBLE_BITS.length];
            long s = DOUBLE_BITS[(r / DOUBLE_BITS.length) % DOUBLE_BITS.length];
            double got = copySign(Double.longBitsToDouble(m), Double.longBitsToDouble(s));
            if (Double.doubleToRawLongBits(got) != refCopySign(m, s)) {
                bad++;
            }
            int fm = FLOAT_BITS[r % FLOAT_BITS.length];
            int fs = FLOAT_BITS[(r / FLOAT_BITS.length) % FLOAT_BITS.length];
            float fgot = copySign(Float.intBitsToFloat(fm), Float.intBitsToFloat(fs));
            if (Float.floatToRawIntBits(fgot) != refCopySign(fm, fs)) {
                bad++;
            }
        }
        return bad;
    }

    static int signumMismatches() {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            long b = r % 3 == 0 ? mix(r) : DOUBLE_BITS[r % DOUBLE_BITS.length];
            double d = Double.longBitsToDouble(b);
            if (d != d && (b & 0x0008000000000000L) == 0) {
                continue; // signalling NaN from mix(): not portable
            }
            if (Double.doubleToRawLongBits(signum(d)) != refSignum(b)) {
                bad++;
            }
            if (Double.doubleToRawLongBits(strictSignum(d)) != refSignum(b)) {
                bad++;
            }
            int fb = FLOAT_BITS[r % FLOAT_BITS.length];
            if (Float.floatToRawIntBits(signum(Float.intBitsToFloat(fb))) != refSignum(fb)) {
                bad++;
            }
        }
        return bad;
    }

    interface Thrower {
        void run();
    }

    static String thrown(Thrower t) {
        try {
            t.run();
            return "none";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static int addI(int a, int b) {
        return Math.addExact(a, b);
    }

    static long addL(long a, long b) {
        return Math.addExact(a, b);
    }

    static int mulI(int a, int b) {
        return Math.multiplyExact(a, b);
    }

    static long mulL(long a, long b) {
        return Math.multiplyExact(a, b);
    }

    static int negI(int a) {
        return Math.negateExact(a);
    }

    static long incL(long a) {
        return Math.incrementExact(a);
    }

    static int toInt(long a) {
        return Math.toIntExact(a);
    }

    static int floorDivI(int a, int b) {
        return Math.floorDiv(a, b);
    }

    static int checkIndex(int i, int n) {
        return java.util.Objects.checkIndex(i, n);
    }

    static int checkFromTo(int from, int to, int n) {
        return java.util.Objects.checkFromToIndex(from, to, n);
    }

    static int checkFromSize(int from, int size, int n) {
        return java.util.Objects.checkFromIndexSize(from, size, n);
    }

    static long warmExact() {
        long sink = 0;
        for (int r = 0; r < WARM; r++) {
            int a = r & 0xffff;
            sink += addI(a, 3) + addL(a, 5L) + mulI(a, 7) + mulL(a, 11L) + negI(a) + incL(a);
            sink += toInt(a) + floorDivI(a, (r & 7) + 1) + checkIndex(a & 7, 8);
            sink += checkFromTo(1, (a & 7) + 1, 8) + checkFromSize(1, a & 3, 8);
        }
        return sink;
    }

    static void time(String name, Runnable body, int ops) {
        long t0 = System.nanoTime();
        body.run();
        long t1 = System.nanoTime();
        System.err.println("row " + name + " " + ((t1 - t0) / ops));
    }

    static volatile long timingSink;

    public static void main(String[] args) {
        fact("int-reverse-1", rev(1));
        fact("int-reverse-12345678", rev(0x12345678));
        fact("int-reverse-0f0f0f0f", rev(0x0f0f0f0f));
        fact("int-reverse-min", rev(Integer.MIN_VALUE));
        fact("long-reverse-1", rev(1L));
        fact("long-reverse-0123456789abcdef", rev(0x0123456789abcdefL));
        fact("long-reverse-neg2", rev(-2L));
        fact("reverse-mismatches", reverseMismatches() + reverseMismatches());

        fact("copysign-d-mismatches", copySignMismatches() + copySignMismatches());
        fact("copysign-d-neg0", hex(copySign(1.0, -0.0)));
        fact("copysign-d-nanpayload", hex(copySign(Double.longBitsToDouble(0x7ff8000000000123L), -1.0)));
        fact("copysign-d-nansign", hex(copySign(1.0, Double.longBitsToDouble(0xfff8000000000000L))));
        fact("strict-copysign-d-nansign", hex(strictCopySign(1.0, Double.longBitsToDouble(0xfff8000000000000L))));
        fact("copysign-f-neg0", hex(copySign(2.5f, -0.0f)));
        fact("copysign-f-nanpayload", hex(copySign(Float.intBitsToFloat(0x7fc00123), -1.0f)));

        fact("signum-mismatches", signumMismatches() + signumMismatches());
        fact("signum-d-neg0", hex(signum(-0.0)));
        fact("signum-d-pos0", hex(signum(0.0)));
        fact("signum-d-nanpayload", hex(signum(Double.longBitsToDouble(0xfff8000000000123L))));
        fact("signum-d-minsub", hex(signum(Double.MIN_VALUE)));
        fact("signum-d-neginf", hex(signum(Double.NEGATIVE_INFINITY)));
        fact("signum-f-neg", hex(signum(-3.5f)));
        fact("signum-f-nan", hex(signum(Float.NaN)));
        fact("strict-signum-d-neg", hex(strictSignum(-1e-300)));

        fact("classify-mismatches", classifyMismatches() + classifyMismatches());
        double[] ds = {
            0.0, -0.0, Double.MAX_VALUE, Double.MIN_VALUE, Double.POSITIVE_INFINITY,
            Double.NEGATIVE_INFINITY, Double.NaN, Double.longBitsToDouble(0xfff8000000000123L),
        };
        StringBuilder cd = new StringBuilder();
        for (double d : ds) {
            cd.append(cd.length() == 0 ? "" : ",").append(classify(d));
        }
        fact("classify-d", cd);
        float[] fs = {
            0.0f, -0.0f, Float.MAX_VALUE, Float.MIN_VALUE, Float.POSITIVE_INFINITY,
            Float.NEGATIVE_INFINITY, Float.NaN, Float.intBitsToFloat(0x7fc00123),
        };
        StringBuilder cf = new StringBuilder();
        for (float f : fs) {
            cf.append(cf.length() == 0 ? "" : ",").append(classify(f));
        }
        fact("classify-f", cf);

        fact("compress-mismatches", compressMismatches() + compressMismatches());
        fact("int-compress-cafebabe-ff00", compressI(0xCAFEBABE, 0xFF00));
        fact("int-expand-ba-ff00", expandI(0xBA, 0xFF00));
        fact("int-compress-neg1-hilo", compressI(-1, 0x80000001));
        fact("int-expand-neg1-hilo", expandI(-1, 0x80000001));
        fact("int-compress-allmask", compressI(0x12345678, -1));
        fact("int-expand-zeromask", expandI(0x12345678, 0));
        fact("int-compress-odd-even", compressI(0x55555555, 0xAAAAAAAA));
        fact("int-compress-neg1-even", compressI(-1, 0xAAAAAAAA));
        fact("int-expand-ffff-odd", expandI(0xFFFF, 0x55555555));
        fact("long-compress-hi32", compressL(0x0123456789ABCDEFL, 0xFFFFFFFF00000000L));
        fact("long-expand-hi32", expandL(0x89ABCDEFL, 0xFFFFFFFF00000000L));
        fact("long-compress-neg1-hilo", compressL(-1L, Long.MIN_VALUE | 1L));
        fact("long-expand-1-min", expandL(1L, Long.MIN_VALUE));
        fact("long-expand-alt", expandL(0x0123456789ABCDEFL, 0xAAAAAAAAAAAAAAAAL));

        long sink = warmExact() + warmExact();
        fact("addexact-int", thrown(() -> addI(Integer.MAX_VALUE, 1)));
        fact("addexact-long", thrown(() -> addL(Long.MIN_VALUE, -1L)));
        fact("multiplyexact-int", thrown(() -> mulI(1 << 16, 1 << 16)));
        fact("multiplyexact-long", thrown(() -> mulL(Long.MIN_VALUE, -1L)));
        fact("negateexact-int", thrown(() -> negI(Integer.MIN_VALUE)));
        fact("incrementexact-long", thrown(() -> incL(Long.MAX_VALUE)));
        fact("tointexact", thrown(() -> toInt(1L << 32)));
        fact("floordiv-zero", thrown(() -> floorDivI(5, 0)));
        fact("checkindex-high", thrown(() -> checkIndex(5, 3)));
        fact("checkindex-neg", thrown(() -> checkIndex(-1, 3)));
        fact("checkindex-neglen", thrown(() -> checkIndex(0, -1)));
        fact("checkfromto", thrown(() -> checkFromTo(2, 1, 5)));
        fact("checkfromsize", thrown(() -> checkFromSize(2, 4, 5)));
        fact("exact-in-range", addI(40, 2) + " " + mulL(-3L, 7L) + " " + toInt(-5L));
        fact("warm", sink != 0);

        if (Boolean.getBoolean("j27.time") || "1".equals(System.getProperty("j27.time"))) {
            final int ops = 2_000_000;
            time("reverse-int", () -> {
                int acc = 0;
                for (int i = 0; i < ops; i++) {
                    acc += rev(i);
                }
                timingSink = acc;
            }, ops);
            time("copysign-d", () -> {
                double acc = 0;
                for (int i = 0; i < ops; i++) {
                    acc += copySign((double) i, (i & 1) == 0 ? 1.0 : -1.0);
                }
                timingSink = (long) acc;
            }, ops);
            time("signum-d", () -> {
                double acc = 0;
                for (int i = 0; i < ops; i++) {
                    acc += signum((double) (i - ops / 2));
                }
                timingSink = (long) acc;
            }, ops);
            time("compress-l", () -> {
                long acc = 0;
                for (int i = 0; i < ops; i++) {
                    acc += compressL(i * 0x9E3779B97F4A7C15L, 0x00FF00FF00FF00FFL);
                }
                timingSink = acc;
            }, ops);
            time("isfinite-d", () -> {
                long acc = 0;
                for (int i = 0; i < ops; i++) {
                    acc += isFinite(i == 7 ? Double.NaN : (double) i) ? 1 : 0;
                }
                timingSink = acc;
            }, ops);
        }
    }
}
