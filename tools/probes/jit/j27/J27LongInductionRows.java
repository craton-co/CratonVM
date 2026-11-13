// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT r27 LOOP27-2 (book docs/feature-designs/native-dispatch-architecture.md):
// a counted `long` induction variable (`for (long i = c; i < n; i += s)`) reaches the `int` loop rules
// (CRATONVM_JIT_IR_NARROW_LONG_INDUCTION, default ON, `=0` kills; `CRATONVM_DBG_UNROLL=1` prints
// `narrowed N long induction(s) to int`).
//
// Facts (deterministic): `warm`, one line per bound n -- where the loop runs out of its array, so the
// compiled, narrowed, vectorized or unrolled body must deopt and report the same index, partial sum and
// exception as HotSpot -- the length-bounded kernels, and the post-warm repeat. Every `fact`/data line
// must equal HotSpot 25's output (java -cp out J27LongInductionRows), both modes, with the switch on and off.
// Timing lines (`time <row> <ns> ns/op`, one op = one call over 1000 elements, best of 7): HotSpot
// 25 on the host: copy 365, weighted 404, abs 446, param 266, nested 2204, fill 325, minmax 815;
// CratonVM before the narrowing 1.5-12 us, after 50-450 ns (abs, not vector-planned, 1.5-1.9 us).

public class J27LongInductionRows {
    static long sumN(int[] a, int n) {
        long i = 0; int s = 0;
        try { for (i = 0; i < n; i++) { s += a[(int) i]; } } catch (ArrayIndexOutOfBoundsException e) { return -(s * 1000L + i); }
        return s * 1000L + i;
    }

    static long stride3(int[] a, int n) {
        long i = 3; int s = 0;
        try { for (i = 3; i < n; i += 3) { s += a[(int) i] * 7; } } catch (ArrayIndexOutOfBoundsException e) { return -(s * 1000L + i); }
        return s * 1000L + i;
    }

    static long weightedN(int[] a, int n) {
        long ls = 0, i = 0;
        try { for (i = 0; i < n; i++) { ls += i * a[(int) i]; } } catch (ArrayIndexOutOfBoundsException e) { return -(ls + i); }
        return ls + i;
    }

    static long lenSum(int[] a) {
        long i; int s = 0;
        for (i = 0; i < a.length; i++) { s += a[(int) i] ^ (int) i; }
        return s * 31L + i;
    }

    static long lenStride2(int[] a) {
        long i; int s = 0;
        for (i = 1; i < a.length; i += 2) { s += a[(int) i] + (int) (i >> 1); }
        return s * 31L + i;
    }

    static int copy(int[] a, int[] b) { for (long i = 0; i < a.length; i++) b[(int) i] = a[(int) i] + 1; return b[3]; }
    static long weighted(int[] a) { long ls = 0; for (long i = 0; i < a.length; i++) ls += i * a[(int) i]; return ls; }
    static int abs(int[] a) { int s = 0; for (long i = 0; i < a.length; i++) s += Math.abs(a[(int) i]); return s; }
    static int param(int[] a, int n) { int s = 0; for (long i = 0; i < n; i++) s += a[(int) i]; return s; }
    static int nested(int[] a) { int s = 0; for (long j = 0; j < 8; j++) for (long i = 0; i < a.length; i++) s += a[(int) i] ^ (int) j; return s; }
    static int fill(int[] a) { for (long i = 0; i < a.length; i++) a[(int) i] = (int) i * 3; return a[5]; }
    static int minmax(int[] a) { int m = 0; for (long i = 0; i < a.length; i++) m = Math.max(m, a[(int) i]); return m; }

    static long sink;

    interface Op {
        long run();
    }

    static void time(String row, Op op) {
        for (int w = 0; w < 30000; w++) sink += op.run();
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < 7; rep++) {
            long t0 = System.nanoTime();
            for (int w = 0; w < 20000; w++) sink += op.run();
            best = Math.min(best, (System.nanoTime() - t0) / 20000);
        }
        System.out.println("time " + row + " " + best + " ns/op");
    }

    public static void main(String[] x) {
        int[] a = new int[100];
        for (int i = 0; i < 100; i++) a[i] = i * 13 - 400;
        int[] e = new int[0];
        long t = 0;
        for (int w = 0; w < 60000; w++) {
            t += sumN(a, 100) + stride3(a, 100) + weightedN(a, 77) + lenSum(a) + lenStride2(a) + lenSum(e) + lenStride2(e);
        }
        System.out.println("warm " + t);
        int[] ns = {0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 99, 100, 101, 105, 1000, Integer.MAX_VALUE, -5};
        for (int n : ns) {
            System.out.println(n + " " + sumN(a, n) + " " + stride3(a, n) + " " + weightedN(a, n));
        }
        System.out.println(lenSum(a) + " " + lenStride2(a) + " " + lenSum(e) + " " + lenStride2(e) + " "
                + lenStride2(new int[2]) + " " + lenSum(new int[9]));
        for (int w = 0; w < 60000; w++) {
            t += sumN(a, 100) + stride3(a, 100) + weightedN(a, 77);
        }
        System.out.println("again " + sumN(a, 100) + " " + sumN(a, 120) + " " + stride3(a, 130) + " " + weightedN(a, 111) + " " + t);

        int[] c = new int[1000], d = new int[1000];
        for (int i = 0; i < 1000; i++) c[i] = i * 7 - 300;
        time("copy", () -> copy(c, d));
        time("weighted", () -> weighted(c));
        time("abs", () -> abs(c));
        time("param", () -> param(c, 1000));
        time("nested", () -> nested(c));
        time("fill", () -> fill(d));
        time("minmax", () -> minmax(c));
        System.out.println("fact kernels " + copy(c, d) + " " + weighted(c) + " " + abs(c) + " " + param(c, 1000) + " "
                + nested(c) + " " + fill(d) + " " + minmax(c));
    }
}
