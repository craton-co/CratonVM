// JIT round 23 wave 1, lane L8 (closing docs/feature-designs/jit-ir-optimizations.md):
// the IR vector pre-loop's shapes -- min/max reductions (VEC21-5), early-exit scans (VEC21-8),
// `!=` loop tests (BCE21-5), element-wise Math.min/max stores (L8-3), the wider scans (L8-4:
// FP element compares, element-tree tests, two-way tests, char order compares) and a `!=` loop
// that starts at a non-zero constant (L8-5).
//
// Rows (TIME lines, ns per call over 1024 elements):
//   imax lmax dmax fmin          m = Math.max/min(m, a[j])
//   findB findC findI            first j with a[j] == x (byte / char / int)
//   sumI neSum                   for (j = 0; j < n / j != n; j++) s += a[j]
//   emaxI eminL emaxD eminF      c[j] = Math.max/min(a[j], b[j])
//   findD findNeD findLtD        first j with a[j] == x / a[j] != x / a[j] < x (double)
//   findMask findDigit           first j with (a[j] & 0xFF) == c (int[]) / a[j] - '0' < 10 (char[])
//   findTwo                      first j with a[j] == c1 || a[j] == c2 (byte[])
//   findAscii                    first j with a[j] >= 0x80 (char[]: the ASCII prefix)
//   sumFrom3                     for (j = 3; j != a.length; j++) s += a[j]
// FACT lines: every kernel's answers over boundary lengths, hit positions, NaN / -0.0 data
// (NaNs printed canonicalised: HotSpot's payload answers are not stable, L8-6). Must equal
// HotSpot 25 line for line.
//
// Run lines (javac -d tools/probes/out tools/probes/jit/r23/R23W1L8VecRows.java):
//   cratonvm -cp tools/probes/out R23W1L8VecRows            # facts + every row
//   cratonvm -cp tools/probes/out R23W1L8VecRows facts      # facts only
//   cratonvm -cp tools/probes/out R23W1L8VecRows imax 200000  # one row, N calls
//   java -cp tools/probes/out R23W1L8VecRows facts          # HotSpot reference

import java.util.Locale;

public class R23W1L8VecRows {
    static final int N = 1024;

    // ── VEC21-5 ──
    static int imax(int[] a, int m, int n) {
        for (int j = 0; j < n; j++) m = Math.max(m, a[j]);
        return m;
    }

    static long lmax(long[] a, long m, int n) {
        for (int j = 0; j < n; j++) m = Math.max(m, a[j]);
        return m;
    }

    static double dmax(double[] a, double m, int n) {
        for (int j = 0; j < n; j++) m = Math.max(m, a[j]);
        return m;
    }

    static float fmin(float[] a, float m, int n) {
        for (int j = 0; j < n; j++) m = Math.min(m, a[j]);
        return m;
    }

    // ── VEC21-8 ──
    static int findB(byte[] a, int c, int n) {
        for (int j = 0; j < n; j++) if (a[j] == c) return j;
        return -1;
    }

    static int findC(char[] a, int c, int n) {
        for (int j = 0; j < n; j++) if (a[j] == c) return j;
        return -1;
    }

    static int findI(int[] a, int c, int n) {
        for (int j = 0; j < n; j++) if (a[j] == c) return j;
        return -1;
    }

    // ── BCE21-5 ──
    static int sumI(int[] a, int n) {
        int s = 0;
        for (int j = 0; j < n; j++) s += a[j];
        return s;
    }

    static int neSum(int[] a, int n) {
        int s = 0;
        for (int j = 0; j != n; j++) s += a[j];
        return s;
    }

    // ── L8-3 ──
    static void emaxI(int[] c, int[] a, int[] b, int n) {
        for (int j = 0; j < n; j++) c[j] = Math.max(a[j], b[j]);
    }

    static void eminL(long[] c, long[] a, long[] b, int n) {
        for (int j = 0; j < n; j++) c[j] = Math.min(a[j], b[j]);
    }

    static void emaxD(double[] c, double[] a, double[] b, int n) {
        for (int j = 0; j < n; j++) c[j] = Math.max(a[j], b[j]);
    }

    static void eminF(float[] c, float[] a, float x, int n) {
        for (int j = 0; j < n; j++) c[j] = Math.min(a[j] * x, a[j]);
    }

    // ── L8-4 ──
    static int findD(double[] a, double x, int n) {
        for (int j = 0; j < n; j++) if (a[j] == x) return j;
        return -1;
    }

    static int findNeD(double[] a, double x, int n) {
        for (int j = 0; j < n; j++) if (a[j] != x) return j;
        return -1;
    }

    static int findLtD(double[] a, double x, int n) {
        for (int j = 0; j < n; j++) if (a[j] < x) return j;
        return -1;
    }

    static int findMask(int[] a, int c, int n) {
        for (int j = 0; j < n; j++) if ((a[j] & 0xFF) == c) return j;
        return -1;
    }

    static int findDigit(char[] a, int n) {
        for (int j = 0; j < n; j++) if (a[j] - '0' < 10) return j;
        return -1;
    }

    static int findTwo(byte[] a, int c1, int c2, int n) {
        for (int j = 0; j < n; j++) if (a[j] == c1 || a[j] == c2) return j;
        return -1;
    }

    static int findAscii(char[] a, int n) {
        int j = 0;
        while (j < n && a[j] < 0x80) j++;
        return j;
    }

    // ── L8-5 ──
    static int sumFrom3(int[] a) {
        int s = 0;
        for (int j = 3; j != a.length; j++) s += a[j];
        return s;
    }

    // ── data ──
    static int[] ia = new int[N], ib = new int[N], ic = new int[N];
    static long[] la = new long[N], lb = new long[N], lc = new long[N];
    static double[] da = new double[N], db = new double[N], dc = new double[N];
    static float[] fa = new float[N], fc = new float[N];
    static byte[] ba = new byte[N];
    static char[] ca = new char[N], cd = new char[N], cs = new char[N];

    static void fill() {
        for (int i = 0; i < N; i++) {
            ia[i] = (i * 7919) ^ 0x5A5A;
            ib[i] = (N - i) * 31 - 9000;
            la[i] = (long) i * 0x1_0000_0001L - (1L << 40);
            lb[i] = -la[i] + i;
            da[i] = (i % 3 == 0 ? -1 : 1) * (i * 1.25 - 300.0);
            db[i] = i * 0.5 - 200.0;
            fa[i] = (float) (i * 0.75 - 100.0);
            ba[i] = (byte) (i % 100);
            ca[i] = (char) ('a' + i % 26);
            cd[i] = (char) ('A' + i % 26);
            cs[i] = (char) ('a' + i % 26);
        }
        ba[N - 1] = 120;
        ca[N - 1] = 'Z';
        cd[N - 1] = '7';
        cs[N - 1] = 'é';
    }

    static long sink;
    // A runtime key (a constant one past `& 0xFF` folds the test away).
    static int maskKey = 0x1FF;

    static String bits(double d) {
        return Double.isNaN(d) ? "NaN" : Long.toHexString(Double.doubleToRawLongBits(d));
    }

    static String bits(float f) {
        return Float.isNaN(f) ? "NaN" : Integer.toHexString(Float.floatToRawIntBits(f));
    }

    // Each fact kernel is driven through enough calls to be compiled before its answers are taken.
    static final int[] LENS = {0, 1, 3, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1000, 1023, 1024};

    static void facts() {
        StringBuilder sb = new StringBuilder();
        for (int rep = 0; rep < 3; rep++) {
            sb.setLength(0);
            long h = 0;
            for (int n : LENS) {
                h = h * 31 + imax(ia, Integer.MIN_VALUE, n);
                h = h * 31 + lmax(la, Long.MIN_VALUE, n);
                h = h * 31 + findB(ba, 99, n) + 7 * findB(ba, 120, n) + 13 * findB(ba, 200, n);
                h = h * 31 + findC(ca, 'z', n) + 7 * findC(ca, 'Z', n);
                h = h * 31 + findI(ia, ia[500], n) + 7 * findI(ia, -1, n);
                h = h * 31 + neSum(ia, n) + 7 * sumI(ia, n);
                h = h * 31 + findMask(ia, ia[700] & 0xFF, n) + 7 * findMask(ia, 0x1FF, n);
                h = h * 31 + findDigit(cd, n) + 7 * findDigit(ca, n);
                h = h * 31 + findTwo(ba, 50, 77, n) + 7 * findTwo(ba, 120, -3, n);
                h = h * 31 + findAscii(cs, n) + 7 * findAscii(ca, n);
                h = h * 31 + findD(da, da[600], n) + 7 * findD(da, 0.1, n);
                h = h * 31 + findNeD(db, db[0], n) + 7 * findLtD(da, -900.0, n) + 13 * findLtD(db, -199.0, n);
                emaxI(ic, ia, ib, n);
                eminL(lc, la, lb, n);
                for (int i = 0; i < N; i++) h = h * 31 + ic[i] + lc[i];
            }
            for (int r = 0; r < 200; r++) {
                for (int n : LENS) {
                    sink += imax(ia, 0, n) + lmax(la, 0, n) + findB(ba, 99, n) + findC(ca, 'Z', n)
                            + findI(ia, -1, n) + neSum(ia, n) + findMask(ia, 0x1FF, n)
                            + findDigit(cd, n) + findTwo(ba, 50, 77, n) + findAscii(cs, n)
                            + findD(da, 0.1, n) + findNeD(db, db[0], n) + findLtD(da, -900.0, n);
                    emaxI(ic, ia, ib, n);
                    eminL(lc, la, lb, n);
                    emaxD(dc, da, db, n);
                    eminF(fc, fa, -1.5f, n);
                    sink += (long) dmax(da, -1e300, n) + (long) fmin(fa, 1e30f, n) + sumFrom3(ia);
                }
            }
            sb.append("FACT int/long h=").append(Long.toHexString(h)).append('\n');
            // FP: -0.0 / +0.0 / NaN placements.
            double[] z = new double[70];
            float[] zf = new float[70];
            long fh = 0;
            for (int p = -1; p < 70; p += 7) {
                for (int i = 0; i < 70; i++) {
                    z[i] = -0.0;
                    zf[i] = 0.0f;
                }
                if (p >= 0) {
                    z[p] = 0.0;
                    zf[p] = -0.0f;
                }
                for (int n : new int[] {0, 1, 4, 5, 16, 17, 33, 70}) {
                    sb.append("FACT dmax0 p=").append(p).append(" n=").append(n).append(' ')
                            .append(bits(dmax(z, -0.0, n))).append(' ')
                            .append(bits(fmin(zf, 0.0f, n))).append('\n');
                }
            }
            double[] nn = new double[70];
            for (int i = 0; i < 70; i++) nn[i] = i - 30.0;
            nn[44] = Double.NaN;
            for (int n : new int[] {0, 1, 44, 45, 70}) {
                sb.append("FACT dmaxNaN n=").append(n).append(' ').append(bits(dmax(nn, -1e9, n)))
                        .append(" findD=").append(findD(nn, Double.NaN, n))
                        .append(" findNeD=").append(findNeD(nn, nn[0], n))
                        .append(" findLtD=").append(findLtD(nn, -100.0, n)).append('\n');
            }
            // Element-wise FP min/max with signed zeros and NaNs.
            double[] ea = {0.0, -0.0, 1.0, Double.NaN, -0.0, 5.0, Double.NEGATIVE_INFINITY, 2.0, 0.0, -1.0};
            double[] eb = {-0.0, 0.0, Double.NaN, 1.0, -0.0, -5.0, 3.0, 2.0, 0.0, Double.POSITIVE_INFINITY};
            double[] xa = new double[40], xb = new double[40], xc = new double[40];
            for (int i = 0; i < 40; i++) {
                xa[i] = ea[i % 10];
                xb[i] = eb[(i * 3) % 10];
            }
            for (int n : new int[] {0, 3, 4, 5, 39, 40}) {
                java.util.Arrays.fill(xc, 9.0);
                emaxD(xc, xa, xb, n);
                StringBuilder row = new StringBuilder();
                for (int i = 0; i < 40; i++) row.append(bits(xc[i])).append(',');
                sb.append("FACT emaxD n=").append(n).append(' ').append(row).append('\n');
                float[] ya = new float[40], yc = new float[40];
                for (int i = 0; i < 40; i++) ya[i] = (float) xa[i];
                eminF(yc, ya, -0.0f, n);
                row.setLength(0);
                for (int i = 0; i < 40; i++) row.append(bits(yc[i])).append(',');
                sb.append("FACT eminF n=").append(n).append(' ').append(row).append('\n');
            }
            // A sumFrom3 over short arrays: lengths below the start never stop (AIOOBE).
            for (int len : new int[] {3, 4, 10, 11, 12, 100}) {
                int[] q = new int[len];
                for (int i = 0; i < len; i++) q[i] = i * 3 + 1;
                sb.append("FACT sumFrom3 len=").append(len).append(' ').append(sumFrom3(q)).append('\n');
            }
            for (int len : new int[] {0, 1, 2}) {
                String v;
                try {
                    v = String.valueOf(sumFrom3(new int[len]));
                } catch (ArrayIndexOutOfBoundsException e) {
                    // The class only: HotSpot's fast throw drops the message once C2 compiles.
                    v = e.getClass().getName();
                }
                sb.append("FACT sumFrom3 len=").append(len).append(' ').append(v).append('\n');
            }
            // A scan whose bound passes the array: AIOOBE at the exact iteration.
            String v;
            try {
                v = String.valueOf(findB(ba, 121, N + 5));
            } catch (ArrayIndexOutOfBoundsException e) {
                v = e.getClass().getName();
            }
            sb.append("FACT findB over ").append(v).append('\n');
        }
        System.out.print(sb);
    }

    interface Row {
        long run();
    }

    static Row row(String name) {
        switch (name) {
            case "imax": return () -> imax(ia, Integer.MIN_VALUE, N);
            case "lmax": return () -> lmax(la, Long.MIN_VALUE, N);
            case "dmax": return () -> (long) dmax(da, -1e300, N);
            case "fmin": return () -> (long) fmin(fa, 1e30f, N);
            case "findB": return () -> findB(ba, 120, N);
            case "findC": return () -> findC(ca, 'Z', N);
            case "findI": return () -> findI(ia, -1, N);
            case "sumI": return () -> sumI(ia, N);
            case "neSum": return () -> neSum(ia, N);
            case "emaxI": return () -> { emaxI(ic, ia, ib, N); return ic[5]; };
            case "eminL": return () -> { eminL(lc, la, lb, N); return lc[5]; };
            case "emaxD": return () -> { emaxD(dc, da, db, N); return (long) dc[5]; };
            case "eminF": return () -> { eminF(fc, fa, -1.5f, N); return (long) fc[5]; };
            case "findD": return () -> findD(da, 0.1, N);
            case "findNeD": return () -> findNeD(db, -999.0, N) + findNeD(da, da[0], N);
            case "findLtD": return () -> findLtD(da, -1e9, N);
            case "findMask": return () -> findMask(ia, maskKey, N);
            case "findDigit": return () -> findDigit(cd, N);
            case "findTwo": return () -> findTwo(ba, 120, -3, N);
            case "findAscii": return () -> findAscii(cs, N);
            case "sumFrom3": return () -> sumFrom3(ia);
            default: throw new IllegalArgumentException(name);
        }
    }

    static final String[] ROWS = {"imax", "lmax", "dmax", "fmin", "findB", "findC", "findI", "sumI", "neSum",
        "emaxI", "eminL", "emaxD", "eminF", "findD", "findNeD", "findLtD", "findMask", "findDigit",
        "findTwo", "findAscii", "sumFrom3"};

    // The timed loop calls `chunk` repeatedly: a method the JIT compiles at
    // entry (with the row's kernel inlined), so no measurement depends on an
    // OSR entry being taken.
    static final int CHUNK = 100;

    static long chunk(Row r, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += r.run();
        return s;
    }

    static void time(String name, int calls) {
        Row r = row(name);
        long s = 0;
        for (int i = 0; i < calls / 4; i += CHUNK) s += chunk(r, CHUNK);
        long best = Long.MAX_VALUE;
        for (int k = 0; k < 5; k++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < calls; i += CHUNK) s += chunk(r, CHUNK);
            best = Math.min(best, System.nanoTime() - t0);
        }
        sink += s;
        System.out.println(String.format(Locale.ROOT, "TIME %s%d %.1f ns/op", name, N, (double) best / calls));
    }

    public static void main(String[] args) {
        fill();
        if (args.length == 0) {
            facts();
            for (String r : ROWS) time(r, 20000);
        } else if (args[0].equals("facts")) {
            facts();
        } else {
            time(args[0], args.length > 1 ? Integer.parseInt(args[1]) : 20000);
        }
        if (sink == 42) System.out.println();
    }
}
