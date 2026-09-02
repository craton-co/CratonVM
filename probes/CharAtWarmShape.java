// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The warm-up shape that DOES reach the inline `String.charAt` lowering.
 *
 * Byte-identical method body to `CharAtCostCurve.scan`. The only difference is
 * three warm calls before the measured one. On CratonVM this printed ~3 ns/char
 * where `CharAtCostCurve` printed ~190-340; on HotSpot both print ~0.5.
 *
 * WHY, settled 2026-09-02 — and the rep count was never it. This class calls
 * its bodies through a `LongSupplier`, so the loop goes hot inside the
 * interpreter and the **OSR door** compiles them; `CharAtCostCurve` calls
 * `scan` straight from `main`, and the **method-entry door** gets there first.
 * `java/lang/String` is `final`, so that door's `final`-class devirtualisation
 * rewrote every `charAt` site to a statically-bound kind and it left the invoke
 * loop before the call-site intrinsic gate could claim it — a real `CALL` into
 * `String.charAt` instead of the inline decode. The OSR door runs no such
 * rewrite, which is the whole of the 100x. See
 * `probes/CharAtDoorProbe.java`, which puts both shapes in one class, and
 * string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.
 *
 * The class comment here used to say "Keep the three warm reps at 200.
 * Dropping them to 50 loses the fast body entirely (measured 340 ns/char)" —
 * which its own `warm3x50` row (3.22-3.44 ns/char, every run since)
 * contradicts. Both readings are real: which door wins is a RACE between the
 * invocation counter and the back-edge counter, and a run where the
 * method-entry door won produced the 340. Since the devirtualisation fix the
 * two doors emit the same decode, so the race no longer decides 100x.
 *
 *   java -cp probes CharAtWarmShape
 *   cratonvm -cp probes CharAtWarmShape
 *   CRATONVM_DBG_JITC=1 cratonvm -cp probes CharAtWarmShape 2>&1 | grep admission
 *
 * See string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901
 */
public class CharAtWarmShape {
    static String big;

    // TWO byte-identical bodies, deliberately. One method warmed both ways
    // would carry whichever body the FIRST arm produced into the second, and
    // the second arm would report it — which is exactly the measurement error
    // this probe exists to avoid. Separate methods get separate artifacts.
    static int scanBig(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static int scanSmall(String s, int reps) {
        int c = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < s.length(); i++)
                if (s.charAt(i) == 'a') c++;
        return c;
    }

    static void run(String n, java.util.function.LongSupplier f, int warm, long chars) {
        for (int i = 0; i < warm; i++) f.getAsLong();
        long t = System.nanoTime();
        long g = f.getAsLong();
        long ns = System.nanoTime() - t;
        System.out.printf("%-14s %8.2f ns/char  %6d ms  g=%d%n", n, (double) ns / chars, ns / 1_000_000, g);
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 100_000; i++) sb.append((char) ('a' + (i % 26)));
        big = sb.toString();

        // Order matters: run the SMALL-warm arm first, so it cannot inherit
        // anything the big-warm arm produced.
        run("warm3x50",  () -> scanSmall(big, 50),  3,  5_000_000L);
        run("warm3x200", () -> scanBig(big, 200),   3, 20_000_000L);
    }
}
