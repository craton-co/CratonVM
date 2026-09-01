// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Cost of `String.charAt` in a counted loop, at growing scale, against the
 * identical loop over a `char[]`.
 *
 * A flat line means one steady state — no warm-up ramp, so whatever body is
 * running at 200,000 characters is still running at 100,000,000. The `char[]`
 * rows are the control: same loop, same bounds check, same induction variable,
 * no `charAt`.
 *
 *   java -cp probes CharAtCostCurve
 *   cratonvm -cp probes CharAtCostCurve
 *   CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 cratonvm -cp probes CharAtCostCurve
 *   CRATONVM_JIT_IR_OVER_INTRINSIC=1       cratonvm -cp probes CharAtCostCurve
 *
 * See docs/known-issues/perf/string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md
 */
public class CharAtCostCurve {
    static String big;

    static int scan(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    /** Same, with `length()` hoisted, to separate the two String calls. */
    static int scanLocalLen(String s, int reps) {
        int c = 0, n = s.length();
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < n; i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    /** The control: identical loop, no String call. */
    static int scanArr(char[] a, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < a.length; i++)
                if (a[i] == 'a') c++;
        return c;
    }

    static void row(String n, int reps, long ns, long g) {
        long chars = (long) reps * 100_000L;
        System.out.printf("%-16s reps=%-6d %9.2f ns/char  %7d ms  g=%d%n",
                n, reps, (double) ns / chars, ns / 1_000_000, g);
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 100_000; i++) sb.append((char) ('a' + (i % 26)));
        big = sb.toString();
        char[] arr = big.toCharArray();

        // The repeated reps=200 rows are deliberate: three identical
        // measurements in a row, so a one-off is distinguishable from a level.
        for (int reps : new int[]{2, 10, 50, 200, 200, 200, 1000}) {
            long t = System.nanoTime(); int g = scan(big, reps);
            row("charAt", reps, System.nanoTime() - t, g);
        }
        for (int reps : new int[]{2, 10, 50, 200, 1000}) {
            long t = System.nanoTime(); int g = scanLocalLen(big, reps);
            row("charAt+localLen", reps, System.nanoTime() - t, g);
        }
        for (int reps : new int[]{2, 10, 50, 200, 1000}) {
            long t = System.nanoTime(); int g = scanArr(arr, reps);
            row("char[]", reps, System.nanoTime() - t, g);
        }
    }
}
