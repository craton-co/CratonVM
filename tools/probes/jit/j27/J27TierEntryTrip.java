// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 1, lane TIER: facts for two tier-up timing fixes.
 *
 *  - burst: a C1 body called 30 000 times in a short burst from a compiled
 *    caller (CRATONVM_JIT_ENTRY_TRIP, opt-in; page
 *    jr21-conf-compiled-bodies-reach-c2-only-at-a-drain-clock-FIXED-20261029.md).
 *  - short-loop: a 40-iteration loop method called 600 times through a caller
 *    that compiles part way (CRATONVM_JIT_BC_CALLEE_METHOD_COUNT, default ON;
 *    page jr21-short-loop-40-act-bimodal-interpreted-FIXED-20261029.md).
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in both modes and in
 * every arm:
 *
 *   fact burst-sum 487475224
 *   fact short-loop-sum 84000
 *
 * Engagement (not stdout): CRATONVM_DBG_TIER_ENQUEUE=1 shows the C2 line of
 * J27TierEntryTrip.kernel(I)I near invocations=20000 with
 * CRATONVM_JIT_ENTRY_TRIP=1, later without it; CRATONVM_DBG_JITC=1 shows a
 * "bc-callee-tiered-enqueue J27TierEntryTrip.tiny40" line before the 500th
 * compiled call of tiny40.
 *
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=1.
 */
public final class J27TierEntryTrip {
    static int kernel(int i) {
        return (i * 31) ^ (i >>> 3);
    }

    static int tiny40(int n) {
        int s = 0;
        for (int i = 0; i < 40; i++) {
            s += (i ^ n) & 7;
        }
        return s;
    }

    static int call(int n) {
        return tiny40(n);
    }

    public static void main(String[] args) {
        boolean time = "1".equals(System.getProperty("j27.time"));
        // Warm kernel to its single-pass body, then one burst.
        long warm = 0;
        for (int i = 0; i < 2_000; i++) {
            warm += kernel(i);
        }
        long burst = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < 30_000; i++) {
            burst += kernel(i) & 0x7FFF;
        }
        long t1 = System.nanoTime();
        if (warm == 42) {
            System.out.println("unreachable");
        }
        System.out.println("fact burst-sum " + burst);
        if (time) {
            System.err.println("row burst " + (t1 - t0) / 30_000);
        }
        long shortSum = 0;
        long ts = 0;
        for (int c = 0; c < 600; c++) {
            if (c == 300) {
                ts = System.nanoTime();
            }
            shortSum += call(c);
        }
        long te = System.nanoTime();
        System.out.println("fact short-loop-sum " + shortSum);
        if (time) {
            System.err.println("row short-loop-40 " + (te - ts) / 300);
        }
    }
}
