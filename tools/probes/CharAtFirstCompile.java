// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Does the FIRST compile of a method decide its body for the life of the
 * process?
 *
 * `a` and `b` are byte-identical. `a`'s first activation is a direct call from
 * `main`; `b`'s first activation is a call from a helper. Both are then
 * measured the same way, several times, with the two interleaved so drift and
 * host load cannot separate them.
 *
 * A VM that re-optimises converges: the two columns end up equal. A VM whose
 * first compile is final keeps them apart forever, and the gap is the cost of
 * the body each one happened to get.
 *
 *   java -cp probes CharAtFirstCompile
 *   cratonvm -cp probes CharAtFirstCompile
 */
public class CharAtFirstCompile {
    static String big;

    static int a(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static int b(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static long viaHelper(int reps) { return b(big, reps); }

    static void row(String n, int reps, long ns, long g) {
        System.out.printf("%-24s %8.2f ns/char  %6d ms  g=%d%n",
                n, (double) ns / ((long) reps * 100_000L), ns / 1_000_000, g);
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 100_000; i++) sb.append((char) ('a' + (i % 26)));
        big = sb.toString();

        // First activation of `a`: straight from main.
        long t = System.nanoTime(); long g = a(big, 50);
        row("a first (from main)", 50, System.nanoTime() - t, g);

        // First activation of `b`: from a helper.
        t = System.nanoTime(); g = viaHelper(50);
        row("b first (from helper)", 50, System.nanoTime() - t, g);

        // Now measure both the SAME way, three times each, interleaved.
        for (int k = 0; k < 3; k++) {
            t = System.nanoTime(); g = a(big, 50);
            row("a rerun " + k, 50, System.nanoTime() - t, g);
            t = System.nanoTime(); g = b(big, 50);
            row("b rerun " + k, 50, System.nanoTime() - t, g);
        }
    }
}
