// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

/**
 * JIT j27 wave 5, lane INTRHUNT: HotSpot-25 parity facts for the edge inputs of
 * the JIT's intrinsic expansions, both tiers (single-pass `JitIntrinsic` arms in
 * jit/src/x64/op_invoke.rs, optimizing-tier `ScalarOp` / pure-expansion / String
 * prefix arms in jit/src/ir_lower.rs and jit/src/ir/pure_expand.rs).
 *
 * Every family is a small static "hot" method holding ONE intrinsic call site,
 * driven over its edge table WARM rounds before the facts are formatted, so the
 * printed answers come from compiled code (int results are returned through a
 * `long` to catch a lost sign extension). NaN results of Math.min/max are
 * printed through floatToIntBits / doubleToLongBits (canonical: HotSpot's own
 * tiers need not agree on WHICH NaN); every other FP answer prints its raw bits.
 *
 * Expected HotSpot 25 output: the page
 * docs/feature-designs/jit-ir-optimizations.md.
 * Deterministic; no timing lines. -Dj27.warm=N overrides the round count.
 */
public final class J27IntrinsicEdgeRows {
    static final int WARM = Integer.getInteger("j27.warm", 8000);
    static long sink;

    /** Accumulates one fact's comma list, or only folds into `sink` while warming. */
    static final class Acc {
        final StringBuilder sb;

        Acc(boolean fmt) {
            sb = fmt ? new StringBuilder() : null;
        }

        void sep() {
            if (sb.length() > 0) {
                sb.append(',');
            }
        }

        void dec(long v) {
            if (sb == null) {
                sink ^= v;
                return;
            }
            sep();
            sb.append(v);
        }

        void hex32(int v) {
            if (sb == null) {
                sink ^= v;
                return;
            }
            sep();
            sb.append(Integer.toHexString(v));
        }

        void hex64(long v) {
            if (sb == null) {
                sink ^= v;
                return;
            }
            sep();
            sb.append(Long.toHexString(v));
        }

        void str(String s) {
            if (sb == null) {
                sink ^= s.length();
                return;
            }
            sep();
            sb.append(s);
        }

        void fact(String name) {
            if (sb != null) {
                System.out.println("fact " + name + " " + sb);
            }
        }
    }

    // ---- integer bit families (results widened to long: an int must stay sign-extended) ----
    static long bitCount(int x) { return Integer.bitCount(x); }
    static long nlz(int x) { return Integer.numberOfLeadingZeros(x); }
    static long ntz(int x) { return Integer.numberOfTrailingZeros(x); }
    static long hob(int x) { return Integer.highestOneBit(x); }
    static long lob(int x) { return Integer.lowestOneBit(x); }
    static long rev(int x) { return Integer.reverse(x); }
    static long rbytes(int x) { return Integer.reverseBytes(x); }
    static long rotlNeg1(int x) { return Integer.rotateLeft(x, -1); }
    static long rotr33(int x) { return Integer.rotateRight(x, 33); }
    static long cmp0(int x) { return Integer.compare(x, 0); }
    static long abs(int x) { return Math.abs(x); }
    static long minNeg7(int x) { return Math.min(x, -7); }
    static long maxNeg7(int x) { return Math.max(x, -7); }

    static long bitCount(long x) { return Long.bitCount(x); }
    static long nlz(long x) { return Long.numberOfLeadingZeros(x); }
    static long ntz(long x) { return Long.numberOfTrailingZeros(x); }
    static long hob(long x) { return Long.highestOneBit(x); }
    static long lob(long x) { return Long.lowestOneBit(x); }
    static long rev(long x) { return Long.reverse(x); }
    static long rbytes(long x) { return Long.reverseBytes(x); }
    static long rotl65(long x) { return Long.rotateLeft(x, 65); }
    static long rotrNeg1(long x) { return Long.rotateRight(x, -1); }
    static long cmp0(long x) { return Long.compare(x, 0L); }
    static long abs(long x) { return Math.abs(x); }
    static long minNeg7(long x) { return Math.min(x, -7L); }
    static long maxNeg7(long x) { return Math.max(x, -7L); }

    static long mulHi(long a, long b) { return Math.multiplyHigh(a, b); }
    static long umulHi(long a, long b) { return Math.unsignedMultiplyHigh(a, b); }

    static final int[] INTS = {0, 1, -1, Integer.MIN_VALUE, Integer.MAX_VALUE, 0x12345678, 0x80, 0x00F00000};
    static final long[] LONGS = {0L, 1L, -1L, Long.MIN_VALUE, Long.MAX_VALUE, 0x0123456789abcdefL, 0x80L};
    static final long[][] MULS = {
        {Long.MIN_VALUE, Long.MIN_VALUE}, {-1L, -1L}, {-1L, 1L}, {Long.MAX_VALUE, Long.MAX_VALUE},
        {Long.MIN_VALUE, -1L}, {123456789012345678L, 987654321098765432L}, {Long.MIN_VALUE, 2L},
    };

    static void intBits(boolean fmt) {
        String[] names = {"bitcount", "nlz", "ntz", "highestonebit", "lowestonebit", "reverse",
            "reversebytes", "rotl-neg1", "rotr-33", "compare0", "abs", "min-neg7", "max-neg7"};
        for (int op = 0; op < names.length; op++) {
            Acc a = new Acc(fmt);
            for (int x : INTS) {
                switch (op) {
                    case 0 -> a.dec(bitCount(x));
                    case 1 -> a.dec(nlz(x));
                    case 2 -> a.dec(ntz(x));
                    case 3 -> a.dec(hob(x));
                    case 4 -> a.dec(lob(x));
                    case 5 -> a.dec(rev(x));
                    case 6 -> a.dec(rbytes(x));
                    case 7 -> a.dec(rotlNeg1(x));
                    case 8 -> a.dec(rotr33(x));
                    case 9 -> a.dec(cmp0(x));
                    case 10 -> a.dec(abs(x));
                    case 11 -> a.dec(minNeg7(x));
                    default -> a.dec(maxNeg7(x));
                }
            }
            a.fact("int-" + names[op]);
        }
    }

    static void longBits(boolean fmt) {
        String[] names = {"bitcount", "nlz", "ntz", "highestonebit", "lowestonebit", "reverse",
            "reversebytes", "rotl-65", "rotr-neg1", "compare0", "abs", "min-neg7", "max-neg7"};
        for (int op = 0; op < names.length; op++) {
            Acc a = new Acc(fmt);
            for (long x : LONGS) {
                switch (op) {
                    case 0 -> a.dec(bitCount(x));
                    case 1 -> a.dec(nlz(x));
                    case 2 -> a.dec(ntz(x));
                    case 3 -> a.dec(hob(x));
                    case 4 -> a.dec(lob(x));
                    case 5 -> a.dec(rev(x));
                    case 6 -> a.dec(rbytes(x));
                    case 7 -> a.dec(rotl65(x));
                    case 8 -> a.dec(rotrNeg1(x));
                    case 9 -> a.dec(cmp0(x));
                    case 10 -> a.dec(abs(x));
                    case 11 -> a.dec(minNeg7(x));
                    default -> a.dec(maxNeg7(x));
                }
            }
            a.fact("long-" + names[op]);
        }
        Acc hi = new Acc(fmt);
        Acc uhi = new Acc(fmt);
        for (long[] p : MULS) {
            hi.dec(mulHi(p[0], p[1]));
            uhi.dec(umulHi(p[0], p[1]));
        }
        hi.fact("multiplyhigh");
        uhi.fact("unsignedmultiplyhigh");
    }

    // ---- floating point ----
    static float minF(float a, float b) { return Math.min(a, b); }
    static float maxF(float a, float b) { return Math.max(a, b); }
    static double minD(double a, double b) { return Math.min(a, b); }
    static double maxD(double a, double b) { return Math.max(a, b); }
    static float absF(float a) { return Math.abs(a); }
    static double absD(double a) { return Math.abs(a); }
    static long roundF(float a) { return Math.round(a); }
    static long roundD(double a) { return Math.round(a); }
    static double floorD(double a) { return Math.floor(a); }
    static double ceilD(double a) { return Math.ceil(a); }
    static double rintD(double a) { return Math.rint(a); }
    static double strictFloorD(double a) { return StrictMath.floor(a); }
    static double strictCeilD(double a) { return StrictMath.ceil(a); }
    static double strictRintD(double a) { return StrictMath.rint(a); }
    static double sqrtD(double a) { return Math.sqrt(a); }
    static double fmaD(double a, double b, double c) { return Math.fma(a, b, c); }
    static float fmaF(float a, float b, float c) { return Math.fma(a, b, c); }

    static final int[][] FPAIRS = {
        {0x00000000, 0x80000000}, {0x80000000, 0x00000000}, {0x80000000, 0x80000000},
        {0x7fc00123, 0x3f800000}, {0x3f800000, 0xffc00456}, {0x3f800000, 0x40000000},
        {0xff800000, 0x7f800000}, {0xbf800000, 0x80000000},
    };
    static final long[][] DPAIRS = {
        {0L, 0x8000000000000000L}, {0x8000000000000000L, 0L},
        {0x8000000000000000L, 0x8000000000000000L}, {0x7ff8000000000123L, 0x3ff0000000000000L},
        {0x3ff0000000000000L, 0xfff8000000000456L}, {0x3ff0000000000000L, 0x4000000000000000L},
        {0xfff0000000000000L, 0x7ff0000000000000L}, {0xbff0000000000000L, 0x8000000000000000L},
    };
    static final int[] ABSF = {0x80000000, 0xffc00123, 0xbf800000, 0xff800000, 0x00000001};
    static final long[] ABSD = {0x8000000000000000L, 0xfff8000000000123L, 0xc004000000000000L,
        0xfff0000000000000L, 0x8000000000000001L};
    static final float[] ROUNDF = {0.49999997f, -0.5f, 0.5f, -2.5f, 2.5f, Float.NaN, 1e10f, -1e10f,
        8388609.0f, -0.0f, Float.MIN_VALUE, Float.MAX_VALUE, -1.5f};
    static final double[] ROUNDD = {0.49999999999999994, -0.5, 2.5, -2.5, 4503599627370497.0, 1e19,
        -1e19, Double.NaN, Double.MIN_VALUE, -Double.MIN_VALUE, -9.223372036854775808E18};
    static final double[] ROUNDINGS = {-0.0, -0.5, 0.5, -1.5, 2.5, -2.5, 3.5, 1e300,
        -Double.MIN_VALUE, 2251799813685248.5};
    static final double[] SQRTS = {-0.0, 2.0, 4.0, Double.POSITIVE_INFINITY, Double.MIN_VALUE, 1e-300};
    static final double[][] FMAD = {{0.1, 10.0, -1.0}, {2.0, 3.0, 4.0}, {-0.0, 1.0, -0.0}, {1.0, 1.0, -1.0}};
    static final float[][] FMAF = {{0.1f, 10f, -1f}, {2f, 3f, 4f}, {-0.0f, 1f, -0.0f}, {1f, 1f, -1f}};

    static void fp(boolean fmt) {
        Acc mnf = new Acc(fmt);
        Acc mxf = new Acc(fmt);
        for (int[] p : FPAIRS) {
            float a = Float.intBitsToFloat(p[0]);
            float b = Float.intBitsToFloat(p[1]);
            mnf.hex32(Float.floatToIntBits(minF(a, b)));
            mxf.hex32(Float.floatToIntBits(maxF(a, b)));
        }
        mnf.fact("minf");
        mxf.fact("maxf");
        Acc mnd = new Acc(fmt);
        Acc mxd = new Acc(fmt);
        for (long[] p : DPAIRS) {
            double a = Double.longBitsToDouble(p[0]);
            double b = Double.longBitsToDouble(p[1]);
            mnd.hex64(Double.doubleToLongBits(minD(a, b)));
            mxd.hex64(Double.doubleToLongBits(maxD(a, b)));
        }
        mnd.fact("mind");
        mxd.fact("maxd");
        Acc af = new Acc(fmt);
        for (int b : ABSF) {
            af.hex32(Float.floatToRawIntBits(absF(Float.intBitsToFloat(b))));
        }
        af.fact("absf");
        Acc ad = new Acc(fmt);
        for (long b : ABSD) {
            ad.hex64(Double.doubleToRawLongBits(absD(Double.longBitsToDouble(b))));
        }
        ad.fact("absd");
        Acc rf = new Acc(fmt);
        for (float f : ROUNDF) {
            rf.dec(roundF(f));
        }
        rf.fact("roundf");
        Acc rd = new Acc(fmt);
        for (double d : ROUNDD) {
            rd.dec(roundD(d));
        }
        rd.fact("roundd");
        String[] names = {"floor", "ceil", "rint", "strict-floor", "strict-ceil", "strict-rint"};
        for (int op = 0; op < names.length; op++) {
            Acc a = new Acc(fmt);
            for (double d : ROUNDINGS) {
                double r = switch (op) {
                    case 0 -> floorD(d);
                    case 1 -> ceilD(d);
                    case 2 -> rintD(d);
                    case 3 -> strictFloorD(d);
                    case 4 -> strictCeilD(d);
                    default -> strictRintD(d);
                };
                a.hex64(Double.doubleToRawLongBits(r));
            }
            a.fact(names[op]);
        }
        Acc sq = new Acc(fmt);
        for (double d : SQRTS) {
            sq.hex64(Double.doubleToRawLongBits(sqrtD(d)));
        }
        sq.fact("sqrt");
        Acc sqn = new Acc(fmt);
        sqn.str(Boolean.toString(Double.isNaN(sqrtD(-1.0))));
        sqn.fact("sqrt-neg1-isnan");
        Acc fd = new Acc(fmt);
        for (double[] t : FMAD) {
            fd.hex64(Double.doubleToRawLongBits(fmaD(t[0], t[1], t[2])));
        }
        fd.fact("fmad");
        Acc ff = new Acc(fmt);
        for (float[] t : FMAF) {
            ff.hex32(Float.floatToRawIntBits(fmaF(t[0], t[1], t[2])));
        }
        ff.fact("fmaf");
    }

    // ---- String / Character ----
    static long idx(String s, int ch) { return s.indexOf(ch); }
    static long idxFrom(String s, int ch, int from) { return s.indexOf(ch, from); }
    static long cmpTo(String a, String b) { return a.compareTo(b); }
    static long cmpToCI(String a, String b) { return a.compareToIgnoreCase(b); }
    static boolean eq(String a, Object b) { return a.equals(b); }
    static long hash(char[] cs) { return new String(cs).hashCode(); }
    static long hashTwice(char[] cs) {
        String s = new String(cs);
        long first = s.hashCode();
        return first ^ (s.hashCode() * 3L);
    }

    static final Object[][] IDX = {
        {"abc", 0x10061}, {"abc", -1}, {"abc", (int) 'c'}, {"été", 0xE9}, {"été", 0x1E9},
        {"x😀", 0x1F600}, {"x\uD83D", 0x1F600}, {"x😀", 0xDE00},
        {"xĀ", 0x110000}, {"xĀ", -1}, {"xĀ", 0x100},
    };
    static final Object[][] IDX_FROM = {
        {"abcabc", (int) 'b', 2}, {"abc", (int) 'a', -5}, {"abc", (int) 'a', 10},
        {"x😀😀", 0x1F600, 2}, {"x😀", 0x1F600, 2},
        {"xĀĀ", 0x100, Integer.MIN_VALUE}, {"abc", 0x161, 0},
    };
    static final String L36 = "abcdefghijklmnopqrstuvwxyz0123456789";
    static final String[][] CMP = {
        {"é", "e"}, {"a", "aĀ"}, {"Ā", "ÿ"}, {"", "abc"}, {"abd", "abc"},
        {"abÿ", "abĀx"}, {"😀", "￿"}, {L36 + "X", L36 + "Y"},
        {L36 + "à", L36 + "a"}, {L36 + "Āà", L36 + "Āa"},
    };
    static final String[][] CMP_CI = {
        {"µ", "a"}, {"ÿ", "a"}, {"ABC", "abd"}, {"À", "à"}, {"ß", "a"},
    };
    static final String[] HASHES = {"polygenelubricants", "f5a5a608", "", "😀",
        "été", "Āabc"};

    static void strings(boolean fmt) {
        Acc a = new Acc(fmt);
        for (Object[] c : IDX) {
            a.dec(idx((String) c[0], (Integer) c[1]));
        }
        a.fact("indexof");
        Acc f = new Acc(fmt);
        for (Object[] c : IDX_FROM) {
            f.dec(idxFrom((String) c[0], (Integer) c[1], (Integer) c[2]));
        }
        f.fact("indexof-from");
        Acc c = new Acc(fmt);
        for (String[] p : CMP) {
            c.dec(cmpTo(p[0], new String(p[1].toCharArray())));
        }
        c.fact("compareto");
        Acc ci = new Acc(fmt);
        for (String[] p : CMP_CI) {
            ci.dec(cmpToCI(p[0], p[1]));
        }
        ci.fact("comparetoignorecase");
        Acc e = new Acc(fmt);
        e.str(Boolean.toString(eq("abc", new String("abc".toCharArray()))));
        e.str(Boolean.toString(eq("abc", "abd")));
        e.str(Boolean.toString(eq("", new String(new char[0]))));
        e.str(Boolean.toString(eq(L36 + L36, new String((L36 + L36).toCharArray()))));
        e.str(Boolean.toString(eq("aĀ", new String("aĀ".toCharArray()))));
        e.str(Boolean.toString(eq("abc", null)));
        e.str(Boolean.toString(eq("abc", new String[] {"abc"})));
        e.str(Boolean.toString(eq("abc", new StringBuilder("abc"))));
        e.fact("equals");
        Acc h = new Acc(fmt);
        Acc h2 = new Acc(fmt);
        for (String s : HASHES) {
            h.dec(hash(s.toCharArray()));
            h2.dec(hashTwice(s.toCharArray()));
        }
        h.fact("hashcode");
        h2.fact("hashcode-twice");
    }

    static boolean isDigit(char c) { return Character.isDigit(c); }
    static boolean isLetter(char c) { return Character.isLetter(c); }
    static boolean isLetterOrDigit(char c) { return Character.isLetterOrDigit(c); }
    static boolean isWhitespace(char c) { return Character.isWhitespace(c); }
    static boolean isUpperCase(char c) { return Character.isUpperCase(c); }
    static boolean isLowerCase(char c) { return Character.isLowerCase(c); }
    static boolean isDigitCp(int c) { return Character.isDigit(c); }
    static boolean isLetterCp(int c) { return Character.isLetter(c); }

    static void chars(boolean fmt) {
        int[] counts = new int[6];
        for (int c = 0; c < 256; c++) {
            char ch = (char) c;
            counts[0] += isDigit(ch) ? 1 : 0;
            counts[1] += isLetter(ch) ? 1 : 0;
            counts[2] += isLetterOrDigit(ch) ? 1 : 0;
            counts[3] += isWhitespace(ch) ? 1 : 0;
            counts[4] += isUpperCase(ch) ? 1 : 0;
            counts[5] += isLowerCase(ch) ? 1 : 0;
        }
        Acc a = new Acc(fmt);
        for (int n : counts) {
            a.dec(n);
        }
        a.fact("char-counts");
        Acc b = new Acc(fmt);
        b.str(Boolean.toString(isLetter('ª')));
        b.str(Boolean.toString(isDigit('²')));
        b.str(Boolean.toString(isWhitespace(' ')));
        b.str(Boolean.toString(isWhitespace('\u001f')));
        b.str(Boolean.toString(isWhitespace('\u0085')));
        b.str(Boolean.toString(isLowerCase('ª')));
        b.str(Boolean.toString(isUpperCase('×')));
        b.str(Boolean.toString(isDigitCp(-1)));
        b.str(Boolean.toString(isLetterCp(0x100)));
        b.str(Boolean.toString(isDigitCp(0x660)));
        b.fact("char-edges");
    }

    // ---- arrays ----
    static boolean eqI(int[] a, int[] b) { return Arrays.equals(a, b); }
    static boolean eqZ(boolean[] a, boolean[] b) { return Arrays.equals(a, b); }
    static boolean eqJ(long[] a, long[] b) { return Arrays.equals(a, b); }
    static boolean eqF(float[] a, float[] b) { return Arrays.equals(a, b); }
    static boolean eqD(double[] a, double[] b) { return Arrays.equals(a, b); }
    static void fillI(int[] a, int v) { Arrays.fill(a, v); }
    static void fillB(byte[] a, byte v) { Arrays.fill(a, v); }
    static void copy(Object s, int sp, Object d, int dp, int n) { System.arraycopy(s, sp, d, dp, n); }

    static void arrays(boolean fmt) {
        int[] x = {1, 2, 3};
        Acc e = new Acc(fmt);
        e.str(Boolean.toString(eqI(null, null)));
        e.str(Boolean.toString(eqI(x, null)));
        e.str(Boolean.toString(eqI(null, x)));
        e.str(Boolean.toString(eqI(new int[0], new int[0])));
        e.str(Boolean.toString(eqI(x, new int[] {1, 2, 3})));
        e.str(Boolean.toString(eqI(x, new int[] {1, 2})));
        e.str(Boolean.toString(eqJ(new long[] {1, 2, 3, 4, 5, Long.MIN_VALUE},
            new long[] {1, 2, 3, 4, 5, Long.MAX_VALUE})));
        e.str(Boolean.toString(eqZ(new boolean[] {true, false}, new boolean[] {true, false})));
        e.str(Boolean.toString(eqF(new float[] {0f}, new float[] {-0f})));
        e.str(Boolean.toString(eqF(new float[] {Float.intBitsToFloat(0x7fc00001)},
            new float[] {Float.intBitsToFloat(0x7fc00002)})));
        e.str(Boolean.toString(eqD(new double[] {Double.longBitsToDouble(0x7ff8000000000001L)},
            new double[] {Double.longBitsToDouble(0xfff8000000000002L)})));
        e.fact("arrays-equals");
        Acc fl = new Acc(fmt);
        int[] fi = new int[37];
        fillI(fi, -2);
        fl.dec(fi[0] + (long) fi[36] + fi.length);
        byte[] fb = new byte[19];
        fillB(fb, (byte) -128);
        fl.dec(fb[0] + (long) fb[18]);
        fillI(new int[0], 5);
        try {
            fillI(null, 1);
            fl.str("no-throw");
        } catch (NullPointerException npe) {
            fl.str(npe.getClass().getName());
        }
        fl.fact("arrays-fill");
        Acc ov = new Acc(fmt);
        int[] a = new int[10];
        for (int i = 0; i < 10; i++) {
            a[i] = i;
        }
        copy(a, 0, a, 1, 9);
        ov.str(Arrays.toString(a).replace(" ", ""));
        copy(a, 1, a, 0, 9);
        ov.str(Arrays.toString(a).replace(" ", ""));
        long[] l = {1, 2, 3, 4, 5, 6};
        copy(l, 1, l, 3, 3);
        ov.str(Arrays.toString(l).replace(" ", ""));
        char[] cs = "abcdef".toCharArray();
        copy(cs, 0, cs, 2, 4);
        ov.str(new String(cs));
        ov.fact("arraycopy-overlap");
    }

    static String copyThrows(Object s, int sp, Object d, int dp, int n) {
        try {
            copy(s, sp, d, dp, n);
            return "no-throw";
        } catch (RuntimeException ex) {
            return ex.getClass().getSimpleName() + ":" + ex.getMessage();
        }
    }

    static void arraycopyThrows(boolean fmt) {
        int[] i4 = new int[4];
        int[] d4 = new int[4];
        String[] rows = {
            copyThrows(i4, -1, d4, 0, 1),
            copyThrows(i4, 0, d4, -1, 1),
            copyThrows(i4, 0, d4, 0, -1),
            copyThrows(i4, 2, d4, 0, 3),
            copyThrows(i4, 0, d4, 3, 2),
            copyThrows(i4, 1, d4, 0, Integer.MAX_VALUE),
            copyThrows(i4, 0, new long[4], 0, 1),
            copyThrows(new String[1], 0, d4, 0, 1),
            copyThrows("str", 0, d4, 0, 1),
            copyThrows(i4, 0, "str", 0, 1),
            copyThrows(new Object[] {1}, 0, new String[1], 0, 1),
            copyThrows(new short[4], -1, new short[4], 0, 1),
            copyThrows(i4, 4, d4, 4, 0),
        };
        for (int k = 0; k < rows.length; k++) {
            Acc a = new Acc(fmt);
            a.str(rows[k]);
            a.fact("arraycopy-throw-" + k);
        }
        Acc n = new Acc(fmt);
        try {
            copy(null, 0, d4, 0, 1);
            n.str("no-throw");
        } catch (NullPointerException npe) {
            n.str(npe.getClass().getName());
        }
        n.fact("arraycopy-null-src");
    }

    static void all(boolean fmt) {
        intBits(fmt);
        longBits(fmt);
        fp(fmt);
        strings(fmt);
        chars(fmt);
        arrays(fmt);
    }

    public static void main(String[] args) {
        for (int r = 0; r < WARM; r++) {
            all(false);
            if ((r & 7) == 0) {
                arraycopyThrows(false);
            }
        }
        all(true);
        arraycopyThrows(true);
        System.err.println("sink " + (sink == 42 ? "x" : "ok"));
    }
}
