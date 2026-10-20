// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Which COMPILE DOOR a `String.charAt` loop takes, decided by nothing but the
 * size of its first call — and what that costs.
 *
 * `CharAtCostCurve.scan` reads ~340 ns/char and `CharAtWarmShape.scanBig`
 * reads ~3 ns/char on one binary, from a byte-identical method body. The
 * charAt page tested five hypotheses about the METHOD (OSR-vs-entry as a
 * property of the method, callee warm order, six caller shapes, first-compile
 * context, scale within one call) and refuted all five. This probe varies the
 * one thing none of them did: **how many iterations the FIRST call runs
 * before anything is compiled.**
 *
 * Four byte-identical methods, differing only in their first call:
 *
 *   direct-from-main — called straight out of `main`, the shape
 *            `CharAtCostCurve` has.
 *   small  — first call is 2 reps (200k chars), through a functional interface.
 *   big    — first call is 200 reps (20M chars), same interface.
 *   ramp   — small first, then big: does the big call REPLACE the body the
 *            small call installed, or inherit it?
 *   big-then-small — the reverse.
 *
 * Every arm is measured with an identical 200-rep call at the end, so the only
 * variable is the history — and the CALLER — that produced its artifact.
 *
 *   java     -cp probes CharAtDoorProbe
 *   cratonvm -cp probes CharAtDoorProbe
 *   CRATONVM_DBG_JITC=1 cratonvm -cp probes CharAtDoorProbe 2>&1 \
 *     | grep -E 'full-compile|OSR-compile|OSR-reuse'
 */
public class CharAtDoorProbe {
    static String big;

    // FOUR byte-identical bodies. One method reused across arms would carry
    // whichever body the first arm produced into the second — the measurement
    // error this probe exists to make visible, so each arm gets its own.
    static int scanSmallFirst(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static int scanBigFirst(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static int scanRamp(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static int scanBigThenSmall(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    interface Body {
        int run(String s, int reps);
    }

    static void measure(String name, Body b, int[] history, int measured) {
        for (int h : history) {
            b.run(big, h);
        }
        long t = System.nanoTime();
        int g = b.run(big, measured);
        long ns = System.nanoTime() - t;
        long chars = (long) measured * 100_000L;
        System.out.printf("%-16s %8.2f ns/char  %7d ms  g=%d%n",
                name, (double) ns / chars, ns / 1_000_000, g);
    }

    /** Called STRAIGHT from `main`, never through an interface. */
    static int scanDirect(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static void row(String name, long ns, int measured, int guard) {
        long chars = (long) measured * 100_000L;
        System.out.printf("%-16s %8.2f ns/char  %7d ms  g=%d%n",
                name, (double) ns / chars, ns / 1_000_000, guard);
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 100_000; i++) sb.append((char) ('a' + (i % 26)));
        big = sb.toString();

        // The direct arm FIRST, and straight out of `main` — the shape
        // `CharAtCostCurve` has. Everything below it is reached through a
        // functional interface, the shape `CharAtWarmShape` has.
        scanDirect(big, 2);
        scanDirect(big, 2);
        scanDirect(big, 2);
        long t = System.nanoTime();
        int g = scanDirect(big, 200);
        row("direct-from-main", System.nanoTime() - t, 200, g);

        // History, then one identical 200-rep measured call for every arm.
        measure("small-first", CharAtDoorProbe::scanSmallFirst, new int[]{2, 2, 2}, 200);
        measure("big-first", CharAtDoorProbe::scanBigFirst, new int[]{200}, 200);
        measure("ramp", CharAtDoorProbe::scanRamp, new int[]{2, 2, 200}, 200);
        measure("big-then-small", CharAtDoorProbe::scanBigThenSmall, new int[]{200, 2, 2}, 200);
    }
}
