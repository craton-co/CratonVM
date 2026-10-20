// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 98, lane L2 (compiled code vs the
// interpreter, the interpreter being the reference, both against HotSpot 25).
// Arithmetic corner cases. Every operation lives in its own small static
// method; `table` runs it over a fixed operand table and folds the answers
// into a hash. The hash is taken once COLD (first calls: the interpreter),
// then each method is warmed past the JIT thresholds (WARM reps of the whole
// table, so 20000+ calls per method and an OSR'd table loop), then taken HOT.
// `cold == hot` must hold in every arm; the hash itself must equal HotSpot's.
// After that, the named corner rows print the hot method's answer for one
// operand pair each (exact values below, from the JLS / JDK 25 javadoc).
//
// A divisor of zero is routed to separate `*Z` methods and hashed by exception
// CLASS only, so HotSpot's -XX:+OmitStackTraceInFastThrow (a hot implicit
// exception preallocated with no message) cannot change the output, and the
// corner rows' `idiv` / `lrem` / `floorDiv` throw for the first time there.
//
// Rows: `hash <op> <hex> <same|DIFF>`, then `<op>(<args>) = <answer>`; an
// exception prints as `!<class>: <message>`.
//
// Run (compile WITHOUT -g; stdout must match HotSpot byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W98JitArithCorners.java
//   $J/java -cp out L2W98JitArithCorners > hs.txt
//   $B --java-home $J/.. -cp out L2W98JitArithCorners | diff hs.txt -
//   $B --java-home $J/.. --nojit -cp out L2W98JitArithCorners | diff hs.txt -
//   (each also with --compatible)
// The hash values are not listed below (compare with HotSpot's run); every
// hash row ends in `same` on HotSpot.
//
// HOTSPOT_EXPECTED_BEGIN (corner rows only, in order)
// idiv(MIN,-1) = -2147483648
// irem(MIN,-1) = 0
// ldiv(MIN,-1) = -9223372036854775808
// lrem(MIN,-1) = 0
// idiv(7,0) = !java.lang.ArithmeticException: / by zero
// lrem(7,0) = !java.lang.ArithmeticException: / by zero
// irem(-7,2) = -1
// irem(7,-2) = 1
// idiv(-7,2) = -3
// ishl(1,32) = 1
// ishl(1,33) = 2
// ishl(1,-1) = -2147483648
// ishr(-8,33) = -4
// iushr(-1,32) = -1
// iushr(-1,33) = 2147483647
// iushr(-1,-1) = 1
// lshl(1,64) = 1
// lshl(1,65) = 2
// lshl(1,-1) = -9223372036854775808
// lushr(-1,65) = 9223372036854775807
// lshr(-8,-63) = -4
// cdiv(-9) = -1 -1 3 9 0 0 -1 1 0
// cdiv(MIN) = -268435456 0 715827882 -2147483648 0 1 0 306783378 -4
// cldiv(-15) = -2 -7 15 15 0 0
// cldiv(MIN) = -1317624576693539401 0 -9223372036854775808 -9223372036854775808 0 0
// cshift(-5) = -10 -1 2147483645 -10 -1 1
// d2i(NaN) = 0
// d2i(Infinity) = 2147483647
// d2i(-Infinity) = -2147483648
// d2i(1.0E10) = 2147483647
// d2i(-1.0E10) = -2147483648
// d2i(-2.5) = -2
// d2l(NaN) = 0
// d2l(9.3E18) = 9223372036854775807
// d2l(-1.0E19) = -9223372036854775808
// d2l(-0.0) = 0
// f2i(NaN) = 0
// f2i(3.0E9) = 2147483647
// f2l(Infinity) = 9223372036854775807
// f2l(-1.0E19) = -9223372036854775808
// l2f(MAX) = 9.223372E18
// l2d(MAX) = 9.223372036854776E18
// l2d(2^53+1) = 9.007199254740992E15
// i2f(2^24+1) = 1.6777216E7
// d2f(1e40) = Infinity
// d2f(-1e-50) = -0.0
// f2d(0.1f) = 0.10000000149011612
// i2b(128) = -128
// i2s(40000) = -25536
// i2c(-1) = 65535
// l2i(2^32+1) = 1
// drem(5.5,-2.0) = 1.5
// drem(-5.5,2.0) = -1.5
// drem(1.0,0.0) = NaN
// drem(Infinity,1.0) = NaN
// drem(1.0,Infinity) = 1.0
// drem(-0.0,1.0) = -0.0
// drem(1e17,3.0) = 1.0
// frem(5.5,-2.0) = 1.5
// frem(-0.0,2.0) = -0.0
// ddiv(1.0,-0.0) = -Infinity
// ddiv(0.0,0.0) = NaN
// dmul(MIN_VALUE,0.5) = 0.0
// dmul(MIN_VALUE,1.5) = 9.9E-324
// dmul(MAX,2.0)/2.0 = Infinity
// fmul(MAX,10)/10 = Infinity
// dadd(0.1,0.2) = 0.30000000000000004
// fcmp(NaN,1.0) = false false false false true false 1
// fcmp(-0.0,0.0) = false false true true false true 1
// abs(MIN) = -2147483648
// labs(MIN) = -9223372036854775808
// dabs(-0.0) = 0.0
// fabs(-0.0) = 0.0
// floorMod(-7,3) = 2
// floorMod(7,-3) = -2
// floorMod(MIN,-1) = 0
// floorDiv(MIN,-1) = -2147483648
// floorDiv(-7,2) = -4
// lfloorMod(-7,3) = 2
// floorDiv(7,0) = !java.lang.ArithmeticException: / by zero
// round(0.5) = 1
// round(-0.5) = 0
// round(2.5) = 3
// round(-2.5) = -2
// round(0.49999999999999994) = 0
// round(NaN) = 0
// round(1e20) = 9223372036854775807
// round(-1e20) = -9223372036854775808
// round(2^52+1) = 4503599627370497
// fround(0.5) = 1
// fround(-0.5) = 0
// fround(-2.5) = -2
// fround(8388609) = 8388609
// fround(1e10) = 2147483647
// rint(0.5) = 0.0
// rint(1.5) = 2.0
// rint(-0.5) = -0.0
// rint(2.5) = 2.0
// floor(-0.0) = -0.0
// floor(-0.5) = -1.0
// ceil(-0.5) = -0.0
// signum(-0.0) = -0.0
// signum(NaN) = NaN
// signum(-3.0) = -1.0
// fsignum(-0.0) = -0.0
// copySign(1.0,-0.0) = -1.0
// fma(0.1,10,-1) = 5.551115123125783E-17
// ffma(0.1,10,-1) = 1.4901161E-8
// fma(Inf,0,1) = NaN
// min(-0.0,0.0) = -0.0
// max(-0.0,0.0) = 0.0
// min(NaN,1.0) = NaN
// max(1.0,NaN) = NaN
// fmin(0.0,-0.0) = -0.0
// sqrt(-0.0) = -0.0
// sqrt(-1.0) = NaN
// multiplyHigh(MAX,MAX) = 4611686018427387903
// multiplyHigh(-1,1) = -1
// addExact(MAX,1) = !java.lang.ArithmeticException
// lmulExact(MAX,2) = !java.lang.ArithmeticException
// toIntExact(2^31) = !java.lang.ArithmeticException: integer overflow
// negateExact(MIN) = !java.lang.ArithmeticException: integer overflow
// absExact(MIN) = !java.lang.ArithmeticException: Overflow to represent absolute value of Integer.MIN_VALUE
// bits(0) = 32 32 64 0 0
// bits(-1) = 0 0 32 -2147483648 -1
// bits(1) = 31 0 63 1 -2147483648
// rot = -2147483648 -9223372036854775808 2018915346
// pow(-0.0,-1.0) = -Infinity
// pow(NaN,0.0) = 1.0
// HOTSPOT_EXPECTED_END
public class L2W98JitArithCorners {
    static final int MIN = Integer.MIN_VALUE;
    static final int MAX = Integer.MAX_VALUE;
    static final long LMIN = Long.MIN_VALUE;
    static final long LMAX = Long.MAX_VALUE;
    static final int WARM = 120;

    static final int[] IV = {0, 1, -1, 2, -2, 3, -3, 7, -7, 8, -8, 9, -9, 31, 32, 33, 63, 64, 65,
        MAX, MIN, MAX - 1, MIN + 1, 0x12345678, -0x12345678, 1000000007};
    static final long[] LV = {0, 1, -1, 2, -2, 3, -3, 7, -7, 8, -8, 15, -15, 63, 64, 65, LMAX, LMIN,
        LMAX - 1, LMIN + 1, MIN, MAX, 0x123456789abcdefL, -0x123456789abcdefL, 1L << 32, (1L << 32) + 1};
    static final double[] DV = {0.0, -0.0, 0.5, -0.5, 1.5, -1.5, 2.5, -2.5, 3.0, -3.0,
        0.49999999999999994, 1e10, -1e10, 2147483647.0, 2147483647.9, 2147483648.0, -2147483648.5,
        -2147483649.0, 9.3e18, -9.3e18, 1e19, -1e19, 1e20, Double.MAX_VALUE, -Double.MAX_VALUE,
        Double.MIN_VALUE, Double.MIN_NORMAL, 4503599627370497.0, 9007199254740993.0, 0.1, 1e300,
        1e-300, 1e17, Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY};
    static final float[] FV = {0.0f, -0.0f, 0.5f, -0.5f, 1.5f, -1.5f, 2.5f, -2.5f, 3.0f, 0.1f,
        8388609.0f, 1e10f, -1e10f, 3e9f, 2147483648.0f, -2147483648.0f, 1e19f, -1e19f,
        Float.MAX_VALUE, Float.MIN_VALUE, Float.MIN_NORMAL, Float.NaN, Float.POSITIVE_INFINITY,
        Float.NEGATIVE_INFINITY};

    // ---- integer division, remainder, shifts --------------------------------
    static int idiv(int a, int b) { return a / b; }
    static int irem(int a, int b) { return a % b; }
    static int ishl(int a, int b) { return a << b; }
    static int ishr(int a, int b) { return a >> b; }
    static int iushr(int a, int b) { return a >>> b; }
    static long ldiv(long a, long b) { return a / b; }
    static long lrem(long a, long b) { return a % b; }
    static long lshl(long a, int b) { return a << b; }
    static long lshr(long a, int b) { return a >> b; }
    static long lushr(long a, int b) { return a >>> b; }

    // Constant divisors and counts (strength-reduced by an optimizing tier).
    static String cdiv(int a) {
        return (a / 8) + " " + (a % 8) + " " + (a / -3) + " " + (a / -1) + " " + (a % -1) + " "
            + (a / MIN) + " " + (a % -8) + " " + (a / 7) / -1 + " " + (a % 1000000007) % 9
            + "";
    }

    static long cdivHash(int a) {
        return (a / 3) * 31L + (a / -3) * 17L + (a / 8) * 13L + (a % 8) * 11L + (a / -1) * 7L
            + (a % -1) * 5L + (a / MIN) * 3L + (a % -8) + (a / 7) * 19L + (a % 1000000007) * 23L
            + (a / 16) * 29L + (a % 16) * 37L + (a / 1) + (a % 1);
    }

    static String cldiv(long a) {
        return (a / 7) + " " + (a % -8) + " " + (a / -1) + " " + (a * -1) + " " + (a % -1) + " "
            + (a / (1L << 4) ) % 2;
    }

    static long cldivHash(long a) {
        return (a / 7) * 31 + (a % -8) * 17 + (a / -1) * 13 + (a % -1) * 11 + (a / 8) * 7
            + (a % 8) * 5 + (a / LMIN) * 3 + (a % 1000000007L) + (a / 3) * 19;
    }

    static String cshift(int a) {
        return (a << 33) + " " + (a >> -1) + " " + (a >>> 33) + " " + ((long) a << 65) + " "
            + ((long) a >> -1) + " " + ((long) a >>> 63);
    }

    // ---- conversions ---------------------------------------------------------
    static int d2i(double d) { return (int) d; }
    static long d2l(double d) { return (long) d; }
    static int f2i(float f) { return (int) f; }
    static long f2l(float f) { return (long) f; }
    static float l2f(long l) { return (float) l; }
    static double l2d(long l) { return (double) l; }
    static float i2f(int i) { return (float) i; }
    static float d2f(double d) { return (float) d; }
    static double f2d(float f) { return (double) f; }
    static int i2b(int i) { return (byte) i; }
    static int i2s(int i) { return (short) i; }
    static int i2c(int i) { return (char) i; }
    static int l2i(long l) { return (int) l; }

    // ---- floating point --------------------------------------------------------
    static double drem(double a, double b) { return a % b; }
    static float frem(float a, float b) { return a % b; }
    static double ddiv(double a, double b) { return a / b; }
    static float fdiv(float a, float b) { return a / b; }
    static double dmul(double a, double b) { return a * b; }
    static float fmul(float a, float b) { return a * b; }
    static double dadd(double a, double b) { return a + b; }
    static double dmulDiv(double a, double b) { return a * b / b; }
    static float fmulDiv(float a, float b) { return a * b / b; }

    static String fcmp(double a, double b) {
        int notLt = 0;
        if (!(a < b)) {
            notLt = 1;
        }
        return (a < b) + " " + (a > b) + " " + (a <= b) + " " + (a >= b) + " " + (a != b) + " "
            + (a == b) + " " + notLt;
    }

    static int fcmpBits(double a, double b) {
        int r = 0;
        if (a < b) r |= 1;
        if (a > b) r |= 2;
        if (a <= b) r |= 4;
        if (a >= b) r |= 8;
        if (a == b) r |= 16;
        if (a != b) r |= 32;
        if (!(a < b)) r |= 64;
        if (!(a >= b)) r |= 128;
        float fa = (float) a;
        float fb = (float) b;
        if (fa < fb) r |= 256;
        if (!(fa > fb)) r |= 512;
        return r + Double.compare(a, b) * 1024;
    }

    // ---- Math ----------------------------------------------------------------------
    static int abs(int a) { return Math.abs(a); }
    static long labs(long a) { return Math.abs(a); }
    static double dabs(double a) { return Math.abs(a); }
    static float fabs(float a) { return Math.abs(a); }
    static int floorMod(int a, int b) { return Math.floorMod(a, b); }
    static int floorDiv(int a, int b) { return Math.floorDiv(a, b); }
    static long lfloorMod(long a, long b) { return Math.floorMod(a, b); }
    static long lfloorDiv(long a, long b) { return Math.floorDiv(a, b); }
    static long round(double a) { return Math.round(a); }
    static int fround(float a) { return Math.round(a); }
    static double rint(double a) { return Math.rint(a); }
    static double floor(double a) { return Math.floor(a); }
    static double ceil(double a) { return Math.ceil(a); }
    static double signum(double a) { return Math.signum(a); }
    static float fsignum(float a) { return Math.signum(a); }
    static double copySign(double a, double b) { return Math.copySign(a, b); }
    static double fma(double a, double b, double c) { return Math.fma(a, b, c); }
    static float ffma(float a, float b, float c) { return Math.fma(a, b, c); }
    static double min(double a, double b) { return Math.min(a, b); }
    static double max(double a, double b) { return Math.max(a, b); }
    static float fmin(float a, float b) { return Math.min(a, b); }
    static float fmax(float a, float b) { return Math.max(a, b); }
    static double sqrt(double a) { return Math.sqrt(a); }
    static long multiplyHigh(long a, long b) { return Math.multiplyHigh(a, b); }
    static int addExact(int a, int b) { return Math.addExact(a, b); }
    static long lmulExact(long a, long b) { return Math.multiplyExact(a, b); }
    static int toIntExact(long a) { return Math.toIntExact(a); }
    static int negateExact(int a) { return Math.negateExact(a); }
    static int absExact(int a) { return Math.absExact(a); }
    static double pow(double a, double b) { return Math.pow(a, b); }

    static String bits(int a) {
        return Integer.numberOfLeadingZeros(a) + " " + Integer.numberOfTrailingZeros(a) + " "
            + Long.numberOfLeadingZeros(a & 0xFFFFFFFFL) + " " + Integer.highestOneBit(a) + " "
            + Integer.reverse(a);
    }

    static long bitsHash(int a) {
        long l = a * 0x9E3779B97F4A7C15L;
        return Integer.numberOfLeadingZeros(a) + 64L * Integer.numberOfTrailingZeros(a)
            + 4096L * Integer.bitCount(a) + Long.numberOfLeadingZeros(l) * 7L
            + Long.numberOfTrailingZeros(l) * 11L + Long.bitCount(l) * 13L
            + Integer.highestOneBit(a) * 17L + Integer.lowestOneBit(a) * 19L
            + Integer.reverse(a) * 23L + Integer.reverseBytes(a) * 29L
            + Integer.rotateLeft(a, a) * 31L + Long.rotateRight(l, a) * 37L
            + Long.reverseBytes(l) * 41L + Character.charCount(a) * 43L
            + Integer.signum(a) * 47L + Long.signum(l) * 53L;
    }

    static String rot() {
        return Integer.rotateLeft(one(), -1) + " " + Long.rotateRight(one(), 65) + " "
            + Integer.reverseBytes(0x12345678 + one() - 1);
    }

    static int one() { return 1; }

    // ---- the table hash ------------------------------------------------------------
    static long mix(long h, long v) {
        return (h ^ v) * 0x100000001B3L + (h >>> 29);
    }

    static long mixD(long h, double v) {
        return mix(h, Double.doubleToLongBits(v));
    }

    static long mixF(long h, float v) {
        return mix(h, Float.floatToIntBits(v));
    }

    static long throwMark(long h, ArithmeticException e) {
        // The class only: a hot implicit exception on HotSpot may be a
        // preallocated one with no message (-XX:+OmitStackTraceInFastThrow).
        return mix(h, 0x5A5A + e.getClass().getName().hashCode());
    }

    // Division by zero lives in its own methods, so the corner rows' `idiv`,
    // `lrem` and `floorDiv` throw for the FIRST time when they print (a message
    // HotSpot keeps), while these throw thousands of times compiled.
    static int idivZ(int a, int b) { return a / b; }
    static int iremZ(int a, int b) { return a % b; }
    static long ldivZ(long a, long b) { return a / b; }
    static long lremZ(long a, long b) { return a % b; }
    static int floorModZ(int a, int b) { return Math.floorMod(a, b); }
    static long lfloorDivZ(long a, long b) { return Math.floorDiv(a, b); }

    static long hashOf(int op) {
        long h = 17;
        switch (op) {
            case 0:
                for (int a : IV) for (int b : IV) {
                    if (b == 0) {
                        try { h = mix(h, idivZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        try { h = mix(h, iremZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        continue;
                    }
                    h = mix(h, idiv(a, b));
                    h = mix(h, irem(a, b));
                }
                return h;
            case 1:
                for (int a : IV) for (int b : IV) {
                    h = mix(h, ishl(a, b));
                    h = mix(h, ishr(a, b));
                    h = mix(h, iushr(a, b));
                }
                return h;
            case 2:
                for (long a : LV) for (long b : LV) {
                    if (b == 0) {
                        try { h = mix(h, ldivZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        try { h = mix(h, lremZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        continue;
                    }
                    h = mix(h, ldiv(a, b));
                    h = mix(h, lrem(a, b));
                }
                return h;
            case 3:
                for (long a : LV) for (int b : IV) {
                    h = mix(h, lshl(a, b));
                    h = mix(h, lshr(a, b));
                    h = mix(h, lushr(a, b));
                }
                return h;
            case 4:
                for (int a : IV) h = mix(h, cdivHash(a));
                for (long a : LV) h = mix(h, cldivHash(a));
                return h;
            case 5:
                for (double d : DV) {
                    h = mix(h, d2i(d));
                    h = mix(h, d2l(d));
                    h = mixF(h, d2f(d));
                }
                for (float f : FV) {
                    h = mix(h, f2i(f));
                    h = mix(h, f2l(f));
                    h = mixD(h, f2d(f));
                }
                for (long l : LV) {
                    h = mixF(h, l2f(l));
                    h = mixD(h, l2d(l));
                    h = mix(h, l2i(l));
                }
                for (int i : IV) {
                    h = mixF(h, i2f(i));
                    h = mix(h, i2b(i));
                    h = mix(h, i2s(i));
                    h = mix(h, i2c(i));
                }
                return h;
            case 6:
                for (double a : DV) for (double b : DV) {
                    h = mixD(h, drem(a, b));
                    h = mixD(h, ddiv(a, b));
                    h = mixD(h, dmul(a, b));
                    h = mixD(h, dadd(a, b));
                    h = mixD(h, dmulDiv(a, b));
                    h = mix(h, fcmpBits(a, b));
                }
                return h;
            case 7:
                for (float a : FV) for (float b : FV) {
                    h = mixF(h, frem(a, b));
                    h = mixF(h, fdiv(a, b));
                    h = mixF(h, fmul(a, b));
                    h = mixF(h, fmulDiv(a, b));
                    h = mixF(h, fmin(a, b));
                    h = mixF(h, fmax(a, b));
                    h = mixF(h, ffma(a, b, 1.0f));
                }
                return h;
            case 8:
                for (double a : DV) {
                    h = mix(h, round(a));
                    h = mixD(h, rint(a));
                    h = mixD(h, floor(a));
                    h = mixD(h, ceil(a));
                    h = mixD(h, signum(a));
                    h = mixD(h, dabs(a));
                    h = mixD(h, sqrt(a));
                    for (double b : DV) {
                        h = mixD(h, min(a, b));
                        h = mixD(h, max(a, b));
                        h = mixD(h, copySign(a, b));
                        h = mixD(h, fma(a, b, 0.5));
                    }
                }
                for (float f : FV) {
                    h = mix(h, fround(f));
                    h = mixF(h, fsignum(f));
                    h = mixF(h, fabs(f));
                }
                return h;
            case 9:
                for (int a : IV) {
                    h = mix(h, abs(a));
                    h = mix(h, bitsHash(a));
                    for (int b : IV) {
                        if (b == 0) {
                            try { h = mix(h, floorModZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        } else {
                            h = mix(h, floorMod(a, b));
                            h = mix(h, floorDiv(a, b));
                        }
                        try { h = mix(h, addExact(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                    }
                }
                for (long a : LV) {
                    h = mix(h, labs(a));
                    try { h = mix(h, toIntExact(a)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                    for (long b : LV) {
                        if (b == 0) {
                            try { h = mix(h, lfloorDivZ(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        } else {
                            h = mix(h, lfloorMod(a, b));
                            h = mix(h, lfloorDiv(a, b));
                        }
                        try { h = mix(h, lmulExact(a, b)); } catch (ArithmeticException e) { h = throwMark(h, e); }
                        h = mix(h, multiplyHigh(a, b));
                    }
                }
                return h;
            default:
                return -1;
        }
    }

    static final String[] OPS = {"idiv/irem", "shifts", "ldiv/lrem", "lshifts", "const-div",
        "conversions", "double", "float", "math-fp", "math-int"};

    // ---- corner rows -----------------------------------------------------------------
    interface Thunk {
        Object get();
    }

    // For an op the table warmed into overflow thousands of times: HotSpot's
    // C2 `Math.*Exact` intrinsics then throw a preallocated exception with no
    // message (-XX:+OmitStackTraceInFastThrow), so only the class is printed.
    static void rowClass(String label, Thunk t) {
        String s;
        try {
            s = String.valueOf(t.get());
        } catch (RuntimeException e) {
            s = "!" + e.getClass().getName();
        }
        System.out.println(label + " = " + s);
    }

    static void row(String label, Thunk t) {
        String s;
        try {
            s = String.valueOf(t.get());
        } catch (RuntimeException e) {
            s = "!" + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(label + " = " + s);
    }

    public static void main(String[] args) {
        long[] cold = new long[OPS.length];
        for (int op = 0; op < OPS.length; op++) {
            cold[op] = hashOf(op);
        }
        for (int r = 0; r < WARM; r++) {
            for (int op = 0; op < OPS.length; op++) {
                if (hashOf(op) != cold[op] && r == WARM - 1) {
                    System.out.println("warm mismatch " + OPS[op]);
                }
            }
            for (int i = -200; i < 200; i++) {
                cdiv(i);
                cldiv(i * 1000003L);
                cshift(i);
                fcmp(i, 3);
                bits(i);
            }
        }
        for (int op = 0; op < OPS.length; op++) {
            long hot = hashOf(op);
            System.out.println("hash " + OPS[op] + " " + Long.toHexString(cold[op]) + " "
                + (hot == cold[op] ? "same" : "DIFF hot=" + Long.toHexString(hot)));
        }

        row("idiv(MIN,-1)", () -> idiv(MIN, -1));
        row("irem(MIN,-1)", () -> irem(MIN, -1));
        row("ldiv(MIN,-1)", () -> ldiv(LMIN, -1));
        row("lrem(MIN,-1)", () -> lrem(LMIN, -1));
        row("idiv(7,0)", () -> idiv(7, 0));
        row("lrem(7,0)", () -> lrem(7, 0));
        row("irem(-7,2)", () -> irem(-7, 2));
        row("irem(7,-2)", () -> irem(7, -2));
        row("idiv(-7,2)", () -> idiv(-7, 2));
        row("ishl(1,32)", () -> ishl(1, 32));
        row("ishl(1,33)", () -> ishl(1, 33));
        row("ishl(1,-1)", () -> ishl(1, -1));
        row("ishr(-8,33)", () -> ishr(-8, 33));
        row("iushr(-1,32)", () -> iushr(-1, 32));
        row("iushr(-1,33)", () -> iushr(-1, 33));
        row("iushr(-1,-1)", () -> iushr(-1, -1));
        row("lshl(1,64)", () -> lshl(1, 64));
        row("lshl(1,65)", () -> lshl(1, 65));
        row("lshl(1,-1)", () -> lshl(1, -1));
        row("lushr(-1,65)", () -> lushr(-1, 65));
        row("lshr(-8,-63)", () -> lshr(-8, -63));
        row("cdiv(-9)", () -> cdiv(-9));
        row("cdiv(MIN)", () -> cdiv(MIN));
        row("cldiv(-15)", () -> cldiv(-15));
        row("cldiv(MIN)", () -> cldiv(LMIN));
        row("cshift(-5)", () -> cshift(-5));

        row("d2i(NaN)", () -> d2i(Double.NaN));
        row("d2i(Infinity)", () -> d2i(Double.POSITIVE_INFINITY));
        row("d2i(-Infinity)", () -> d2i(Double.NEGATIVE_INFINITY));
        row("d2i(1.0E10)", () -> d2i(1e10));
        row("d2i(-1.0E10)", () -> d2i(-1e10));
        row("d2i(-2.5)", () -> d2i(-2.5));
        row("d2l(NaN)", () -> d2l(Double.NaN));
        row("d2l(9.3E18)", () -> d2l(9.3e18));
        row("d2l(-1.0E19)", () -> d2l(-1e19));
        row("d2l(-0.0)", () -> d2l(-0.0));
        row("f2i(NaN)", () -> f2i(Float.NaN));
        row("f2i(3.0E9)", () -> f2i(3e9f));
        row("f2l(Infinity)", () -> f2l(Float.POSITIVE_INFINITY));
        row("f2l(-1.0E19)", () -> f2l(-1e19f));
        row("l2f(MAX)", () -> l2f(LMAX));
        row("l2d(MAX)", () -> l2d(LMAX));
        row("l2d(2^53+1)", () -> l2d((1L << 53) + 1));
        row("i2f(2^24+1)", () -> i2f((1 << 24) + 1));
        row("d2f(1e40)", () -> d2f(1e40));
        row("d2f(-1e-50)", () -> d2f(-1e-50));
        row("f2d(0.1f)", () -> f2d(0.1f));
        row("i2b(128)", () -> i2b(128));
        row("i2s(40000)", () -> i2s(40000));
        row("i2c(-1)", () -> i2c(-1));
        row("l2i(2^32+1)", () -> l2i((1L << 32) + 1));

        row("drem(5.5,-2.0)", () -> drem(5.5, -2.0));
        row("drem(-5.5,2.0)", () -> drem(-5.5, 2.0));
        row("drem(1.0,0.0)", () -> drem(1.0, 0.0));
        row("drem(Infinity,1.0)", () -> drem(Double.POSITIVE_INFINITY, 1.0));
        row("drem(1.0,Infinity)", () -> drem(1.0, Double.POSITIVE_INFINITY));
        row("drem(-0.0,1.0)", () -> drem(-0.0, 1.0));
        row("drem(1e17,3.0)", () -> drem(1e17, 3.0));
        row("frem(5.5,-2.0)", () -> frem(5.5f, -2.0f));
        row("frem(-0.0,2.0)", () -> frem(-0.0f, 2.0f));
        row("ddiv(1.0,-0.0)", () -> ddiv(1.0, -0.0));
        row("ddiv(0.0,0.0)", () -> ddiv(0.0, 0.0));
        row("dmul(MIN_VALUE,0.5)", () -> dmul(Double.MIN_VALUE, 0.5));
        row("dmul(MIN_VALUE,1.5)", () -> dmul(Double.MIN_VALUE, 1.5));
        row("dmul(MAX,2.0)/2.0", () -> dmulDiv(Double.MAX_VALUE, 2.0));
        row("fmul(MAX,10)/10", () -> fmulDiv(Float.MAX_VALUE, 10f));
        row("dadd(0.1,0.2)", () -> dadd(0.1, 0.2));
        row("fcmp(NaN,1.0)", () -> fcmp(Double.NaN, 1.0));
        row("fcmp(-0.0,0.0)", () -> fcmp(-0.0, 0.0));

        row("abs(MIN)", () -> abs(MIN));
        row("labs(MIN)", () -> labs(LMIN));
        row("dabs(-0.0)", () -> dabs(-0.0));
        row("fabs(-0.0)", () -> fabs(-0.0f));
        row("floorMod(-7,3)", () -> floorMod(-7, 3));
        row("floorMod(7,-3)", () -> floorMod(7, -3));
        row("floorMod(MIN,-1)", () -> floorMod(MIN, -1));
        row("floorDiv(MIN,-1)", () -> floorDiv(MIN, -1));
        row("floorDiv(-7,2)", () -> floorDiv(-7, 2));
        row("lfloorMod(-7,3)", () -> lfloorMod(-7, 3));
        row("floorDiv(7,0)", () -> floorDiv(7, 0));
        row("round(0.5)", () -> round(0.5));
        row("round(-0.5)", () -> round(-0.5));
        row("round(2.5)", () -> round(2.5));
        row("round(-2.5)", () -> round(-2.5));
        row("round(0.49999999999999994)", () -> round(0.49999999999999994));
        row("round(NaN)", () -> round(Double.NaN));
        row("round(1e20)", () -> round(1e20));
        row("round(-1e20)", () -> round(-1e20));
        row("round(2^52+1)", () -> round(4503599627370497.0));
        row("fround(0.5)", () -> fround(0.5f));
        row("fround(-0.5)", () -> fround(-0.5f));
        row("fround(-2.5)", () -> fround(-2.5f));
        row("fround(8388609)", () -> fround(8388609.0f));
        row("fround(1e10)", () -> fround(1e10f));
        row("rint(0.5)", () -> rint(0.5));
        row("rint(1.5)", () -> rint(1.5));
        row("rint(-0.5)", () -> rint(-0.5));
        row("rint(2.5)", () -> rint(2.5));
        row("floor(-0.0)", () -> floor(-0.0));
        row("floor(-0.5)", () -> floor(-0.5));
        row("ceil(-0.5)", () -> ceil(-0.5));
        row("signum(-0.0)", () -> signum(-0.0));
        row("signum(NaN)", () -> signum(Double.NaN));
        row("signum(-3.0)", () -> signum(-3.0));
        row("fsignum(-0.0)", () -> fsignum(-0.0f));
        row("copySign(1.0,-0.0)", () -> copySign(1.0, -0.0));
        row("fma(0.1,10,-1)", () -> fma(0.1, 10.0, -1.0));
        row("ffma(0.1,10,-1)", () -> ffma(0.1f, 10.0f, -1.0f));
        row("fma(Inf,0,1)", () -> fma(Double.POSITIVE_INFINITY, 0.0, 1.0));
        row("min(-0.0,0.0)", () -> min(-0.0, 0.0));
        row("max(-0.0,0.0)", () -> max(-0.0, 0.0));
        row("min(NaN,1.0)", () -> min(Double.NaN, 1.0));
        row("max(1.0,NaN)", () -> max(1.0, Double.NaN));
        row("fmin(0.0,-0.0)", () -> fmin(0.0f, -0.0f));
        row("sqrt(-0.0)", () -> sqrt(-0.0));
        row("sqrt(-1.0)", () -> sqrt(-1.0));
        row("multiplyHigh(MAX,MAX)", () -> multiplyHigh(LMAX, LMAX));
        row("multiplyHigh(-1,1)", () -> multiplyHigh(-1, 1));
        rowClass("addExact(MAX,1)", () -> addExact(MAX, 1));
        rowClass("lmulExact(MAX,2)", () -> lmulExact(LMAX, 2));
        row("toIntExact(2^31)", () -> toIntExact(1L << 31));
        row("negateExact(MIN)", () -> negateExact(MIN));
        row("absExact(MIN)", () -> absExact(MIN));
        row("bits(0)", () -> bits(0));
        row("bits(-1)", () -> bits(-1));
        row("bits(1)", () -> bits(1));
        row("rot", () -> rot());
        row("pow(-0.0,-1.0)", () -> pow(-0.0, -1.0));
        row("pow(NaN,0.0)", () -> pow(Double.NaN, 0.0));
    }
}
