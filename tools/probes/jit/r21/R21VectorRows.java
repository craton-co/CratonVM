// JIT round 21 wave 4, lane vector: the loop vectorizer and the SIMD bulk loops -- correctness
// facts for every hazard the lane reviewed, plus time rows for the loops HotSpot's SuperWord
// vectorizes.
//
// Every row method is first called WARM times on small inputs (so both tiers compile it, with the
// vector pre-loop where it applies), then once more on the row's own inputs; the row prints what
// that last call left behind: checksums of whole arrays, accumulator bits (floatToRawIntBits /
// doubleToRawLongBits, so a reassociated FP sum or a lost -0.0 / NaN shows), the induction value
// after the loop, and for a loop that throws the exception class, its message (the index), and
// the destination array's contents around the faulting iteration (earlier stores done, later not).
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21VectorRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx1g -cp $P R21VectorRows                       # jdk-only (launcher default)
//   cratonvm --compatible -Xmx1g -cp $P R21VectorRows          # both modes, same lines
//   java -Xmx1g -cp $P R21VectorRows                           # HotSpot 25 (C2 SuperWord)
// Arms (cratonvm; every non-`time` line must be identical across all of them and HotSpot):
//   (default)                                 round-16 element-wise pre-loop (+ - * /, one kind)
//   CRATONVM_JIT_IR_VECTOR_LOOP=0             kill switch of the IR pre-loop (control)
//   CRATONVM_JIT_IR_VECTOR_BITOPS=1           VEC21-1 (opt-in): & | ^ and constant shifts, int/long;
//                                             moves time int-bitops-map-1024, long-and-map-1024
//   CRATONVM_JIT_IR_VECTOR_SUBWORD=1          VEC21-3 (opt-in): byte / short / char lanes;
//                                             moves time byte-xor-map-1024
//   CRATONVM_JIT_IR_VECTOR_REDUCE=1           VEC21-4 (opt-in): int/long add reductions;
//                                             moves time int-sum-1024, int-dot-1024; float-sum-1024 must NOT move
//   all three =1 together                     the full r21 surface
//   CRATONVM_JIT_VECTORIZE=0                  single-pass SIMD detectors' kill switch (control)
//   --nojit                                   interpreter reference
// Engagement (NOT for timing): CRATONVM_DBG_JITC=1 prints `[ir-vec] loop at bci ..: <Kind> xN
// lanes, ..` per admitted loop (`, reduction` for VEC21-4) and `[ir-vec] loop at bci .. refused:
// <why>` per refused one -- the carried-dependence, mixed-type, FP-sum and min/max rows must show
// a refusal, never an admission.
//
// HotSpot expectation: every non-`time` line identical. Time rows (ns per call over a 1024-element
// array, HotSpot 25 C2 on an AVX2 host, approximate): int-add-map-1024 ~60-90, int-sum-1024 ~40-70,
// int-dot-1024 ~80-130, byte-xor-map-1024 ~20-40, float-sum-1024 ~900-1100 (strict order: C2 does
// not vectorize it either), long-and-map-1024 ~150-250, char-widen-copy-1024 ~70-120,
// int-bitops-map-1024 ~70-110.

public class R21VectorRows {
    static final int WARM = 30_000;

    static long ck(int[] a) {
        long h = 17;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + a[i];
        }
        return h;
    }

    static long ck(long[] a) {
        long h = 19;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + a[i];
        }
        return h;
    }

    static long ck(byte[] a) {
        long h = 23;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + a[i];
        }
        return h;
    }

    static long ck(short[] a) {
        long h = 29;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + a[i];
        }
        return h;
    }

    static long ck(char[] a) {
        long h = 31;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + a[i];
        }
        return h;
    }

    static long ck(float[] a) {
        long h = 37;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + Float.floatToRawIntBits(a[i]);
        }
        return h;
    }

    static long ck(double[] a) {
        long h = 41;
        for (int i = 0; i < a.length; i++) {
            h = h * 1_000_003 + Double.doubleToRawLongBits(a[i]);
        }
        return h;
    }

    static int[] ints(int n, int seed) {
        int[] a = new int[n];
        int x = seed;
        for (int i = 0; i < n; i++) {
            x = x * 1_103_515_245 + 12_345;
            a[i] = x ^ (x >>> 13);
        }
        return a;
    }

    static long[] longs(int n, int seed) {
        long[] a = new long[n];
        long x = seed;
        for (int i = 0; i < n; i++) {
            x = x * 6_364_136_223_846_793_005L + 1_442_695_040_888_963_407L;
            a[i] = x ^ (x >>> 29);
        }
        return a;
    }

    static byte[] bytes(int n, int seed) {
        byte[] a = new byte[n];
        int[] s = ints(n, seed);
        for (int i = 0; i < n; i++) {
            a[i] = (byte) s[i];
        }
        return a;
    }

    static short[] shorts(int n, int seed) {
        short[] a = new short[n];
        int[] s = ints(n, seed);
        for (int i = 0; i < n; i++) {
            a[i] = (short) s[i];
        }
        return a;
    }

    static char[] chars(int n, int seed) {
        char[] a = new char[n];
        int[] s = ints(n, seed);
        for (int i = 0; i < n; i++) {
            a[i] = (char) s[i];
        }
        a[0] = 0xFFFF;
        return a;
    }

    static void line(String row, Object v) {
        System.out.println(row + " " + v);
    }

    // ------------------------------------------------------------ carried dependences (must not vectorize)

    // a[i] = a[i-1] + x: distance-1 recurrence.
    static void prevPlus(int[] a, int n, int x) {
        for (int i = 1; i < n; i++) {
            a[i] = a[i - 1] + x;
        }
    }

    // a[i+k] = a[i] * 3 + 1 with k unknown: forward distance k (k = 0 is element-wise).
    static void fwdK(int[] a, int n, int k) {
        for (int i = 0; i < n; i++) {
            a[i + k] = a[i] * 3 + 1;
        }
    }

    // a[i] = a[i+k] - 7: a backward read, the WAR direction.
    static void backK(int[] a, int n, int k) {
        for (int i = 0; i < n; i++) {
            a[i] = a[i + k] - 7;
        }
    }

    // c[i] = c[i] * c[i] + a[i], called with a == c too.
    static void sameArray(int[] c, int[] a, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = c[i] * c[i] + a[i];
        }
    }

    // double lanes, b == c aliasing through two parameters.
    static void daxpyAlias(double[] c, double[] b, double x, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = c[i] + x * b[i];
        }
    }

    static void carriedRows() {
        for (int w = 0; w < WARM; w++) {
            prevPlus(new int[8], 8, w);
            fwdK(new int[24], 8, w & 7);
            backK(new int[24], 8, w & 7);
            int[] t = ints(8, w);
            sameArray(t, t, 8);
            double[] d = new double[8];
            daxpyAlias(d, d, 1.5, 8);
        }
        int[] a = ints(100, 1);
        prevPlus(a, 100, 3);
        line("carried-prev", ck(a));
        for (int k : new int[] {0, 1, 3, 7, 8, 9, 31, 33}) {
            int[] f = ints(140, 2);
            fwdK(f, 100, k);
            int[] b = ints(140, 3);
            backK(b, 100, k);
            line("carried-k" + k, ck(f) + " " + ck(b));
        }
        int[] c = ints(77, 4);
        int[] c2 = c.clone();
        sameArray(c, ints(77, 5), 77);
        sameArray(c2, c2, 77);
        line("same-array", ck(c) + " " + ck(c2));
        double[] d = new double[45];
        for (int i = 0; i < d.length; i++) {
            d[i] = i == 9 ? -0.0 : i == 13 ? Double.NaN : i * 0.375 - 4.0;
        }
        daxpyAlias(d, d, 1.25, 45);
        line("daxpy-alias", ck(d));
    }

    // ------------------------------------------------------------ mixed element types

    static void widenBS(int[] d, byte[] b, short[] s, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = b[i] + s[i];
        }
    }

    static void widenIL(long[] d, int[] a, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = a[i] * 3L;
        }
    }

    static void charWiden(int[] d, char[] c, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = c[i];
        }
    }

    static void floatWiden(double[] d, float[] f, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = f[i] * 0.5;
        }
    }

    static void mixedRows() {
        for (int w = 0; w < WARM; w++) {
            widenBS(new int[8], new byte[8], new short[8], 8);
            widenIL(new long[8], new int[8], 8);
            charWiden(new int[8], new char[8], 8);
            floatWiden(new double[8], new float[8], 8);
        }
        int[] d = new int[71];
        widenBS(d, bytes(71, 6), shorts(71, 7), 71);
        line("mixed-byte-short-int", ck(d));
        long[] l = new long[53];
        widenIL(l, ints(53, 8), 53);
        line("mixed-int-long", ck(l));
        int[] e = new int[66];
        charWiden(e, chars(66, 9), 66);
        line("mixed-char-widen", ck(e));
        float[] f = new float[29];
        for (int i = 0; i < f.length; i++) {
            f[i] = i == 3 ? Float.NaN : i == 4 ? -0.0f : i == 5 ? Float.MIN_VALUE : i * 1.1f;
        }
        double[] g = new double[29];
        floatWiden(g, f, 29);
        line("mixed-float-double", ck(g));
    }

    // ------------------------------------------------------------ trip counts, bounds, alignment

    static int nearMax(int[] a) {
        int c = 0;
        for (int j = Integer.MAX_VALUE - 3; j < a.length; j++) {
            a[j] = 1;
            c++;
        }
        return c;
    }

    static void offsetMap(int[] c, int[] a, int[] b, int s, int n) {
        for (int j = s; j < n; j++) {
            c[j] = a[j] + b[j] * 5;
        }
    }

    static int tail3(long[] c, int n) {
        int j;
        for (j = n - 3; j < n; j++) {
            c[j] = c[j] * 7 - j;
        }
        return j;
    }

    static void boundRows() {
        for (int w = 0; w < WARM; w++) {
            nearMax(new int[4]);
            offsetMap(new int[16], new int[16], new int[16], w & 3, 16);
            tail3(new long[8], 8);
        }
        int[] big = new int[64];
        line("trip-near-max", nearMax(big) + " " + ck(big));
        long h = 0;
        for (int s = 0; s < 9; s++) {
            for (int n = s; n < 80; n += 7) {
                int[] c = new int[n + 1];
                offsetMap(c, ints(n + 1, s), ints(n + 1, s + 50), s, n);
                h = h * 31 + ck(c);
            }
        }
        line("unaligned-starts", h);
        long[] t = longs(19, 10);
        int j = tail3(t, 19);
        line("tail3", j + " " + ck(t));
    }

    // ------------------------------------------------------------ FP reductions (strict order)

    static float fsum(float[] a, int n) {
        float s = 0.0f;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    static float fsumNeg0(float[] a, int n) {
        float s = -0.0f;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    static double dsum(double[] a, int n) {
        double s = 0.0;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    static double ddot(double[] a, double[] b, int n) {
        double s = 0.0;
        for (int i = 0; i < n; i++) {
            s += a[i] * b[i];
        }
        return s;
    }

    static void fpRows() {
        for (int w = 0; w < WARM; w++) {
            fsum(new float[8], 8);
            fsumNeg0(new float[8], 8);
            dsum(new double[8], 8);
            ddot(new double[8], new double[8], 8);
        }
        float[] f = new float[64];
        for (int i = 0; i < f.length; i++) {
            f[i] = i == 0 ? 1.0e8f : i == 33 ? -1.0e8f : 1.0f + (i % 5) * 0.1f;
        }
        line("float-sum", Integer.toHexString(Float.floatToRawIntBits(fsum(f, 64))));
        float[] z = new float[40];
        java.util.Arrays.fill(z, -0.0f);
        line("float-sum-neg0", Integer.toHexString(Float.floatToRawIntBits(fsumNeg0(z, 40)))
                + " " + Integer.toHexString(Float.floatToRawIntBits(fsum(z, 40))));
        double[] d = new double[64];
        for (int i = 0; i < d.length; i++) {
            d[i] = i % 4 == 0 ? 1.0e16 : i % 4 == 2 ? -1.0e16 : 1.0;
        }
        line("double-sum", Long.toHexString(Double.doubleToRawLongBits(dsum(d, 64))));
        double[] e = new double[50];
        for (int i = 0; i < e.length; i++) {
            e[i] = 1.0 / (i + 1) - (i % 3) * 0.3;
        }
        line("double-dot", Long.toHexString(Double.doubleToRawLongBits(ddot(e, d, 50))));
        f[17] = Float.NaN;
        line("float-sum-nan", Integer.toHexString(Float.floatToRawIntBits(fsum(f, 64))));
    }

    // ------------------------------------------------------------ min / max (NaN, -0.0)

    static float fmax(float[] a, int n) {
        float m = Float.NEGATIVE_INFINITY;
        for (int i = 0; i < n; i++) {
            m = Math.max(m, a[i]);
        }
        return m;
    }

    static float fmin(float[] a, int n) {
        float m = Float.POSITIVE_INFINITY;
        for (int i = 0; i < n; i++) {
            m = Math.min(m, a[i]);
        }
        return m;
    }

    static double dmaxMap(double[] c, double[] a, double[] b, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = Math.max(a[i], b[i]);
        }
        return c[n - 1];
    }

    static int imax(int[] a, int n) {
        int m = Integer.MIN_VALUE;
        for (int i = 0; i < n; i++) {
            m = Math.max(m, a[i]);
        }
        return m;
    }

    static void minMaxRows() {
        for (int w = 0; w < WARM; w++) {
            fmax(new float[8], 8);
            fmin(new float[8], 8);
            dmaxMap(new double[8], new double[8], new double[8], 8);
            imax(new int[8], 8);
        }
        float[] f = new float[33];
        for (int i = 0; i < f.length; i++) {
            f[i] = (i % 2 == 0) ? 0.0f : -0.0f;
        }
        line("fmax-zeros", Integer.toHexString(Float.floatToRawIntBits(fmax(f, 33))));
        line("fmin-zeros", Integer.toHexString(Float.floatToRawIntBits(fmin(f, 33))));
        f[20] = Float.NaN;
        line("fmax-nan", Integer.toHexString(Float.floatToRawIntBits(fmax(f, 33))));
        line("fmin-nan", Integer.toHexString(Float.floatToRawIntBits(fmin(f, 33))));
        double[] a = new double[27];
        double[] b = new double[27];
        for (int i = 0; i < a.length; i++) {
            a[i] = i % 3 == 0 ? -0.0 : i % 3 == 1 ? Double.NaN : i;
            b[i] = i % 2 == 0 ? 0.0 : -i;
        }
        double[] c = new double[27];
        dmaxMap(c, a, b, 27);
        line("dmax-map", ck(c));
        line("imax", imax(ints(99, 11), 99));
    }

    // ------------------------------------------------------------ shifts (Java count masking)

    static void shiftsInt(int[] c, int[] a, int n, int k) {
        for (int i = 0; i < n; i++) {
            c[i] = (a[i] << 35) ^ (a[i] >> 33) ^ (a[i] >>> 37) ^ (a[i] << k);
        }
    }

    static void shiftsLong(long[] c, long[] a, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = (a[i] << 65) + (a[i] >> 70) - (a[i] >>> 127);
        }
    }

    static void shiftsSub(byte[] b, short[] s, char[] c, int n) {
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (b[i] >> 1);
        }
        for (int i = 0; i < n; i++) {
            s[i] = (short) (s[i] >>> 4);
        }
        for (int i = 0; i < n; i++) {
            c[i] = (char) (c[i] >> 3);
        }
    }

    static void byteUshr(byte[] d, byte[] b, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = (byte) (b[i] >>> 1);
        }
    }

    static void shiftRows() {
        for (int w = 0; w < WARM; w++) {
            shiftsInt(new int[8], new int[8], 8, w);
            shiftsLong(new long[8], new long[8], 8);
            shiftsSub(new byte[8], new short[8], new char[8], 8);
            byteUshr(new byte[8], new byte[8], 8);
        }
        long h = 0;
        for (int k : new int[] {0, 1, 31, 32, 33, 40, -1, Integer.MIN_VALUE}) {
            int[] c = new int[61];
            shiftsInt(c, ints(61, 12), 61, k);
            h = h * 31 + ck(c);
        }
        line("shift-int", h);
        long[] l = new long[37];
        shiftsLong(l, longs(37, 13), 37);
        line("shift-long", ck(l));
        byte[] b = bytes(70, 14);
        short[] s = shorts(70, 15);
        char[] c = chars(70, 16);
        shiftsSub(b, s, c, 70);
        line("shift-subword", ck(b) + " " + ck(s) + " " + ck(c));
        byte[] d = new byte[70];
        byteUshr(d, bytes(70, 17), 70);
        line("shift-byte-ushr", ck(d));
    }

    // ------------------------------------------------------------ sub-word sign / zero extension

    static void subwordOps(short[] s, short[] t, char[] c, byte[] b, int n) {
        for (int i = 0; i < n; i++) {
            s[i] = (short) (s[i] * t[i]);
        }
        for (int i = 0; i < n; i++) {
            c[i] = (char) (c[i] + 1);
        }
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (b[i] + 200);
        }
    }

    static void charMinusShort(int[] d, char[] c, short[] s, int n) {
        for (int i = 0; i < n; i++) {
            d[i] = c[i] - s[i];
        }
    }

    static void byteXor(byte[] c, byte[] a, byte[] b, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = (byte) (a[i] ^ b[i]);
        }
    }

    static void charMulAdd(char[] c, char[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = (char) (a[i] * k + c[i]);
        }
    }

    static void subwordRows() {
        for (int w = 0; w < WARM; w++) {
            subwordOps(new short[8], new short[8], new char[8], new byte[8], 8);
            charMinusShort(new int[8], new char[8], new short[8], 8);
            byteXor(new byte[40], new byte[40], new byte[40], 40);
            charMulAdd(new char[20], new char[20], w, 20);
        }
        short[] s = shorts(83, 18);
        char[] c = chars(83, 19);
        byte[] b = bytes(83, 20);
        subwordOps(s, shorts(83, 21), c, b, 83);
        line("subword-ops", ck(s) + " " + ck(c) + " " + ck(b));
        int[] d = new int[83];
        charMinusShort(d, chars(83, 22), shorts(83, 23), 83);
        line("char-minus-short", ck(d));
        long h = 0;
        for (int n : new int[] {0, 1, 31, 32, 33, 127, 128, 129, 200}) {
            byte[] x = new byte[200];
            byteXor(x, bytes(200, 24), bytes(200, 25), n);
            h = h * 31 + ck(x);
        }
        line("byte-xor", h);
        h = 0;
        for (int k : new int[] {3, -1, 65_537, Integer.MIN_VALUE + 5}) {
            char[] x = chars(77, 26);
            charMulAdd(x, chars(77, 27), k, 77);
            h = h * 31 + ck(x);
        }
        line("char-mul-add", h);
    }

    // ------------------------------------------------------------ integer reductions

    static int isum(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    static int idot(int[] a, int[] b, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i] * b[i];
        }
        return s;
    }

    static long lsubSum(long[] a, int n) {
        long s = 1;
        for (int i = 0; i < n; i++) {
            s -= a[i];
        }
        return s;
    }

    static int isumAfter(int[] a, int n) {
        int s = 7;
        int j;
        for (j = 2; j < n; j++) {
            s += a[j];
        }
        return s * 31 + j;
    }

    static void reduceRows() {
        for (int w = 0; w < WARM; w++) {
            isum(new int[16], 16);
            idot(new int[16], new int[16], 16);
            lsubSum(new long[8], 8);
            isumAfter(new int[16], 16);
        }
        int[] a = ints(1000, 28);
        for (int i = 0; i < 64; i++) {
            a[i] = Integer.MAX_VALUE - i;
        }
        StringBuilder sb = new StringBuilder();
        for (int n : new int[] {0, 7, 8, 9, 31, 32, 33, 1000}) {
            sb.append(isum(a, n)).append(',');
        }
        line("int-sum", sb);
        line("int-dot", idot(a, ints(1000, 29), 1000));
        line("long-sub-sum", lsubSum(longs(77, 30), 77));
        line("int-sum-after", isumAfter(a, 1000) + " " + isumAfter(a, 1) + " " + isumAfter(a, 2));
    }

    // ------------------------------------------------------------ exceptions inside the range

    static void addOne(int[] c, int[] a, int s, int n) {
        for (int j = s; j < n; j++) {
            c[j] = a[j] + 1;
        }
    }

    static int SUM;

    static void sumToField(int[] a, int n) {
        SUM = 0;
        for (int j = 0; j < n; j++) {
            SUM += a[j];
        }
    }

    static String exRow(int[] c, int[] a, int s, int n) {
        try {
            addOne(c, a, s, n);
            return "ok";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void exceptionRows() {
        for (int w = 0; w < WARM; w++) {
            addOne(new int[40], new int[40], 0, 40);
            sumToField(new int[40], 40);
        }
        // Destination shorter than the bound: AIOOBE at c.length, every earlier store done.
        int[] c = new int[37];
        String r = exRow(c, ints(64, 31), 0, 50);
        line("aioobe-dst", r + " | " + ck(c) + " c[36]=" + c[36]);
        // Source shorter: AIOOBE at a.length reading, c[0..19] written, c[20..] untouched.
        int[] c2 = new int[50];
        java.util.Arrays.fill(c2, -5);
        r = exRow(c2, ints(20, 32), 0, 50);
        line("aioobe-src", r + " | " + ck(c2) + " c[19]=" + c2[19] + " c[20]=" + c2[20]);
        // Bound far past both, start mid-array.
        int[] c3 = new int[100];
        r = exRow(c3, ints(100, 33), 13, Integer.MAX_VALUE);
        line("aioobe-maxbound", r + " | " + ck(c3) + " c[12]=" + c3[12] + " c[99]=" + c3[99]);
        // Negative start: nothing written.
        int[] c4 = new int[50];
        r = exRow(c4, ints(50, 34), -1, 50);
        line("aioobe-neg-start", r + " | " + ck(c4));
        // Null source with a positive trip: NPE before any store.
        int[] c5 = new int[50];
        r = exRow(c5, null, 0, 50);
        line("npe-src", r.startsWith("java.lang.NullPointerException") + " | " + ck(c5));
        // A reduction that throws: the partial sum is visible in the field.
        try {
            sumToField(ints(45, 35), 60);
            line("aioobe-reduce", "ok");
        } catch (ArrayIndexOutOfBoundsException e) {
            line("aioobe-reduce", e.getMessage() + " | " + SUM);
        }
    }

    // ------------------------------------------------------------ safepoints in long vector loops

    static volatile boolean STOP;

    static void mul3(int[] c, int[] a, int n) {
        for (int j = 0; j < n; j++) {
            c[j] = a[j] * 3 + 1;
        }
    }

    static void safepointRows() throws InterruptedException {
        int n = 1 << 20;
        int[] a = ints(n, 36);
        int[] c = new int[n];
        for (int w = 0; w < 200; w++) {
            mul3(new int[64], a, 64);
        }
        // A second thread asks for a stop (a GC, a stack walk) every couple of
        // milliseconds while the main thread is inside million-element vector
        // loops: each stop must be honoured (the loop's poll) and leave the
        // arrays exactly as the scalar loop would.
        Thread gc = new Thread(() -> {
            while (!STOP) {
                System.gc();
                Thread.getAllStackTraces();
                try {
                    Thread.sleep(2);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.setDaemon(true);
        gc.start();
        long h = 0;
        for (int r = 0; r < 10; r++) {
            mul3(c, a, n);
            h = h * 31 + ck(c) + isum(c, n);
        }
        STOP = true;
        gc.join();
        line("safepoint-long", h);
    }

    // ------------------------------------------------------------ deopt right after the loop

    static int LATE;

    static final class Late {
        static {
            LATE = 99;
        }
    }

    static int loopThenRare(int[] c, int[] a, int n, boolean rare) throws Exception {
        int j;
        int s = 0;
        for (j = 0; j < n; j++) {
            c[j] = a[j] - 3;
        }
        for (int i = 0; i < n; i++) {
            s += c[i];
        }
        if (rare) {
            // An uncommon trap / class load just after both loops.
            Class.forName("R21VectorRows$Late");
            return j * 1000 + s + LATE;
        }
        return j * 1000 + s;
    }

    static void deoptRows() throws Exception {
        int[] a = ints(90, 37);
        for (int w = 0; w < WARM; w++) {
            loopThenRare(new int[90], a, 90, false);
        }
        int[] c = new int[90];
        line("deopt-after-loop", loopThenRare(c, a, 90, true) + " " + ck(c));
    }

    // ------------------------------------------------------------ the r21 opt-in shapes

    static void bitopsMap(int[] c, int[] a, int[] b, int k, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = (((a[i] << 35) >>> 29) ^ b[i]) & k;
        }
    }

    static void longAndMap(long[] c, long[] a, long[] b, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = (a[i] & b[i]) ^ (a[i] >>> 7);
        }
    }

    static void optInRows() {
        for (int w = 0; w < WARM; w++) {
            bitopsMap(new int[16], new int[16], new int[16], w, 16);
            longAndMap(new long[8], new long[8], new long[8], 8);
        }
        long h = 0;
        for (int k : new int[] {-1, 0x7F0FF0F7, 0xFFFF}) {
            int[] c = new int[45];
            bitopsMap(c, ints(45, 38), ints(45, 39), k, 45);
            int[] d = ints(45, 40);
            bitopsMap(d, ints(45, 41), d, k, 45);
            h = h * 31 + ck(c) + ck(d);
        }
        line("bitops-map", h);
        long[] l = new long[23];
        longAndMap(l, longs(23, 42), longs(23, 43), 23);
        line("long-and-map", ck(l));
    }

    // ------------------------------------------------------------ time rows

    static final int REPS = 9;
    static final int CALLS = 5_000;
    static final int LEN = 1024;
    static long sink;

    interface Op {
        long run();
    }

    static final int[] TA = ints(LEN, 50);
    static final int[] TB = ints(LEN, 51);
    static final int[] TC = new int[LEN];
    static final byte[] BA = bytes(LEN, 52);
    static final byte[] BB = bytes(LEN, 53);
    static final byte[] BC = new byte[LEN];
    static final long[] LA = longs(LEN, 54);
    static final long[] LB = longs(LEN, 55);
    static final long[] LC = new long[LEN];
    static final char[] CA = chars(LEN, 56);
    static final float[] FA = new float[LEN];

    static {
        for (int i = 0; i < LEN; i++) {
            FA[i] = (i % 7) * 0.5f - 1.0f;
        }
    }

    static void addMap(int[] c, int[] a, int[] b, int n) {
        for (int i = 0; i < n; i++) {
            c[i] = a[i] + b[i];
        }
    }

    static void time(String name, Op op) {
        for (int w = 0; w < 3; w++) {
            for (int k = 0; k < CALLS; k++) {
                sink += op.run();
            }
        }
        long[] t = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < CALLS; k++) {
                sink += op.run();
            }
            t[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(t);
        System.out.println("time " + name + " " + (t[REPS / 2] / CALLS) + " ns/op");
    }

    static void timeRows() {
        time("int-add-map-1024", () -> {
            addMap(TC, TA, TB, LEN);
            return TC[5];
        });
        time("int-sum-1024", () -> isum(TA, LEN));
        time("int-dot-1024", () -> idot(TA, TB, LEN));
        time("byte-xor-map-1024", () -> {
            byteXor(BC, BA, BB, LEN);
            return BC[7];
        });
        time("float-sum-1024", () -> Float.floatToRawIntBits(fsum(FA, LEN)));
        time("long-and-map-1024", () -> {
            longAndMap(LC, LA, LB, LEN);
            return LC[3];
        });
        time("char-widen-copy-1024", () -> {
            charWiden(TC, CA, LEN);
            return TC[9];
        });
        time("int-bitops-map-1024", () -> {
            bitopsMap(TC, TA, TB, 0x7F0FF0F7, LEN);
            return TC[11];
        });
        System.out.println("sink " + (sink != 0x7fffffffL));
    }

    public static void main(String[] args) throws Exception {
        carriedRows();
        mixedRows();
        boundRows();
        fpRows();
        minMaxRows();
        shiftRows();
        subwordRows();
        reduceRows();
        exceptionRows();
        safepointRows();
        deoptRows();
        optInRows();
        timeRows();
    }
}
