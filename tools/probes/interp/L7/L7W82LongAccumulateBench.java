// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L7: ns per array element for
//   CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG  the long and widening forms
//       of the fused array accumulate in the `lload_N` arms
//       (i73-L7-proposal-long-and-wide-local-forms-of-the-array-accumulate-
//       20261106, item 1; default off)
//
// Rows (CALLS = 2000 calls of a loop over N = 1024 elements each; every
// method keeps its locals in 0..3, the only placement the fused group takes):
//   longSum    `long s; s += la[i]` over a `long[]`
//              (`lload_1; aload_0; iload_3; laload; ladd; lstore_1`). On: a
//              hit, expected down like the wave-73 int rows (~30%).
//   widenSum   `long s; s += a[i]` over an `int[]` of large values
//              (`... iaload; i2l; ladd; lstore_1`; the sum overflows an int,
//              so a wrong widening shows in the checksum). On: a hit.
//   longLocal  `long s; s += i` (`lload_1; iload_3; i2l; ladd; lstore_1`):
//              the `lload_N` arm's screen fails on its first byte test. The
//              off-cost control: flat.
//   intSum     `int s; s += a[i]` (the wave-73 int form, `iload_N` arm).
//              Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare env sets on the SAME
// binary, interleaved, several rounds, and take the median of the per-round
// minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W82LongAccumulateBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// longSum checksum=1047552000
// widenSum checksum=4398045461504000
// longLocal checksum=1047552000
// intSum checksum=1047552000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W82LongAccumulateBench {
    interface Row {
        long run();
    }

    static final int REPS = 9;
    static final int CALLS = 2000;
    static final int N = 1024;

    // Locals: la 0, s 1..2, i 3.
    static long longSum(long[] la) {
        long s = 0;
        for (int i = 0; i < la.length; i++) {
            s += la[i];
        }
        return s;
    }

    // Locals: a 0, s 1..2, i 3.
    static long widenSum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    // Locals: n 0, s 1..2, i 3.
    static long longLocal(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    // Locals: a 0, s 1, i 2.
    static int intSum(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static void time(String name, Row row) {
        long checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ((long) CALLS * N), REPS);
    }

    public static void main(String[] args) {
        long[] la = new long[N];
        int[] big = new int[N];
        int[] small = new int[N];
        for (int k = 0; k < N; k++) {
            la[k] = k;
            big[k] = Integer.MAX_VALUE - k;
            small[k] = k;
        }
        time("longSum", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += longSum(la);
            }
            return t;
        });
        time("widenSum", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += widenSum(big);
            }
            return t;
        });
        time("longLocal", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += longLocal(N);
            }
            return t;
        });
        time("intSum", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += intSum(small);
            }
            return t;
        });
    }
}
