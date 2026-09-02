// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The cost of one bounds-checked array element read in a counted loop, and how
 * much of it is the loop-invariant `a.length` the emitter re-evaluates every
 * iteration.
 *
 * Companion to `CharAtCostCurve`, whose `char[]` control arm
 * (`scanArr`) is the row
 * `docs/known-issues/perf/array-element-load-baseline-codegen-20260901.md`
 * is written about. That page reads the emitter and attributes ~5 of a
 * 21-instruction body to the un-hoisted `arraylength` and its null check. This
 * probe is the arm that prices the same thing from OUTSIDE, and it does so on a
 * binary that predates the fix: each `*Len` row is the identical loop with
 * `a.length` lifted into a local BY HAND, which is what the compiler's own
 * hoist should converge to. The gap between a row and its `*Len` sibling is the
 * whole prize, measured before any of it is claimed.
 *
 * The `int[]` rows exist for the page's second, un-traced witness
 * (`Bench.array_sum`, quoted at 69x). `sum` is the shape `simd.rs`'s
 * `detect_int_array_sum` looks for and `count` deliberately is not, so a large
 * split between them says the AVX2 reduction took one and not the other —
 * which is the question that page left open.
 *
 *   java     -cp probes ArrayElemLoadCost
 *   cratonvm -cp probes ArrayElemLoadCost
 *   CRATONVM_DISABLE_ARRAYLEN_LICM=1 cratonvm -cp probes ArrayElemLoadCost
 */
public class ArrayElemLoadCost {

    /** `char[]` compare-and-count: `CharAtCostCurve.scanArr`, verbatim. */
    static int charScan(char[] a, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < a.length; i++)
                if (a[i] == 'a') c++;
        return c;
    }

    /** Same loop, `a.length` hoisted by hand. The ceiling for the JIT's hoist. */
    static int charScanLen(char[] a, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++) {
            int n = a.length;
            for (int i = 0; i < n; i++)
                if (a[i] == 'a') c++;
        }
        return c;
    }

    /** `int[]` reduction — the shape `detect_int_array_sum` matches. */
    static int intSum(int[] a, int reps) {
        int s = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < a.length; i++)
                s += a[i];
        return s;
    }

    /** Same, hand-hoisted. */
    static int intSumLen(int[] a, int reps) {
        int s = 0;
        for (int r = 0; r < reps; r++) {
            int n = a.length;
            for (int i = 0; i < n; i++)
                s += a[i];
        }
        return s;
    }

    /** `int[]` compare-and-count — deliberately NOT a reduction shape. */
    static int intCount(int[] a, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < a.length; i++)
                if (a[i] == 7) c++;
        return c;
    }

    /** Same, hand-hoisted. */
    static int intCountLen(int[] a, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++) {
            int n = a.length;
            for (int i = 0; i < n; i++)
                if (a[i] == 7) c++;
        }
        return c;
    }

    static final int N = 100_000;

    static void row(String name, int reps, long ns, long guard) {
        long elems = (long) reps * N;
        System.out.printf("%-14s reps=%-5d %9.2f ns/elem  %7d ms  g=%d%n",
                name, reps, (double) ns / elems, ns / 1_000_000, guard);
    }

    public static void main(String[] args) {
        char[] ca = new char[N];
        int[] ia = new int[N];
        for (int i = 0; i < N; i++) {
            ca[i] = (char) ('a' + (i % 26));
            ia[i] = i % 26;
        }

        // Two warm-up reps then three measured ones at the same size, so a
        // single outlier is distinguishable from a level (the convention
        // `CharAtCostCurve` uses, for the same reason).
        //
        // The measured size is an argument because the default is too SHORT to
        // measure with on a shared host: at 100 reps a row runs for ~10 ms, and
        // three runs of one unchanged binary spread 0.52 to 0.92 ns/elem --
        // wider than the effect any single codegen change here has. Pass a
        // larger rep count (2000 gives ~1 s a row) when the number has to carry
        // an A/B rather than a shape.
        int measured = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        int[] plan = {2, 10, measured, measured, measured};

        for (int reps : plan) {
            long t = System.nanoTime();
            int g = charScan(ca, reps);
            row("char[]", reps, System.nanoTime() - t, g);
        }
        for (int reps : plan) {
            long t = System.nanoTime();
            int g = charScanLen(ca, reps);
            row("char[]+len", reps, System.nanoTime() - t, g);
        }
        for (int reps : plan) {
            long t = System.nanoTime();
            int g = intSum(ia, reps);
            row("int[] sum", reps, System.nanoTime() - t, g);
        }
        for (int reps : plan) {
            long t = System.nanoTime();
            int g = intSumLen(ia, reps);
            row("int[] sum+len", reps, System.nanoTime() - t, g);
        }
        for (int reps : plan) {
            long t = System.nanoTime();
            int g = intCount(ia, reps);
            row("int[] cnt", reps, System.nanoTime() - t, g);
        }
        for (int reps : plan) {
            long t = System.nanoTime();
            int g = intCountLen(ia, reps);
            row("int[] cnt+len", reps, System.nanoTime() - t, g);
        }
    }
}
