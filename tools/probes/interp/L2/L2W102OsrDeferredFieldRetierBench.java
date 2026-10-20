// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i5 wave 102, lane L2.
// `CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER` (default OFF; page
// i92-L2-deferred-field-sites-only-at-the-method-entry-door).
//
// `Loop.run(n, phase)` is called a few times only (never hot enough for a
// method-entry body), each call spinning one long loop, so every call is served
// by the OSR door. The loop reads `Cold.BASE` only from iteration `phase` on:
// when the first call's loop goes OSR, `Cold` is not loaded, so the OSR body
// DEFERS the site (wave 93) and the second phase pays the deferred helper on
// every iteration. Without the switch that OSR body is reused by every later
// call for good. With it, the site's first link marks the cached OSR body stale
// and asks one background OSR compile, so from (about) the second call on the
// loop runs a body with the field row.
//   osr-cold     Loop  (its `Cold` loaded by the first call's second phase)
//   osr-control  LoopC (its `ColdC` loaded and initialised before any call)
//
// A/B (one binary; interleave the arms, take medians; --jdk-only and --compatible):
//   $J/javac -d out tools/probes/interp/L2/L2W102OsrDeferredFieldRetierBench.java
//   A (off)  $B --java-home $J/.. -cp out L2W102OsrDeferredFieldRetierBench
//   B (on)   CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER=1 $B --java-home $J/.. -cp out L2W102OsrDeferredFieldRetierBench
//   $J/java -cp out L2W102OsrDeferredFieldRetierBench    (reference)
// Expected: A: osr-cold slower than osr-control (helper call per iteration in
// the second phase); B: osr-cold within noise of osr-control.
//
// Census (B):
//   CRATONVM_DBG_JITC=1 CRATONVM_JIT_OSR_DEFERRED_FIELD_SITE_RETIER=1 $B ... 2>&1 \
//     | grep -E 'osr deferred-field-site|osr field site unresolved|OSR-(compile|reuse)' | sort | uniq -c
// expects `osr deferred-field-sites L2W102OsrDeferredFieldRetierBench$Loop.run(II)I: 1 site(s)`,
// `osr deferred-field-site retier armed ... entry_bci=<b> compile_id=<n>`, then
// `osr deferred-field-site retier ... compile_id=<n>: queued`. No `armed` line:
// the deferral did not happen (the owner was loaded at compile time) or the
// background compiler is off (`CRATONVM_BG_COMPILE=0` disarms the switch).
//
// Timing lines start with "TIME "; the remaining lines are deterministic and
// identical on HotSpot:
//   check-cold <x>
//   check-control <x>
public class L2W102OsrDeferredFieldRetierBench {
    static final int N = 4_000_000;
    static final int CALLS = 9;

    static class Cold {
        static int BASE = 1100;
    }

    static class ColdC {
        static int BASE = 1100;
    }

    static class Loop {
        static int run(int n, int phase) {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s = s * 31 + i;
                if (i >= phase) {
                    s += Cold.BASE;
                }
            }
            return s;
        }
    }

    static class LoopC {
        static int run(int n, int phase) {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s = s * 31 + i;
                if (i >= phase) {
                    s += ColdC.BASE;
                }
            }
            return s;
        }
    }

    static long median(long[] xs) {
        long[] c = xs.clone();
        java.util.Arrays.sort(c);
        return c[c.length / 2];
    }

    public static void main(String[] args) throws Exception {
        if (ColdC.BASE != 1100) {
            System.out.println("!");
        }
        int phase = N / 4;
        long[] cold = new long[CALLS];
        long[] control = new long[CALLS];
        int sumCold = 0;
        int sumControl = 0;
        for (int c = 0; c < CALLS; c++) {
            long t0 = System.nanoTime();
            sumCold += Loop.run(N, phase);
            cold[c] = System.nanoTime() - t0;
            t0 = System.nanoTime();
            sumControl += LoopC.run(N, phase);
            control[c] = System.nanoTime() - t0;
            if (c == 0) {
                // Room for the background OSR recompile the first call asked.
                Thread.sleep(300);
            }
        }
        // The first call of each arm is the compile; the rest are timed.
        long[] coldTail = java.util.Arrays.copyOfRange(cold, 1, CALLS);
        long[] controlTail = java.util.Arrays.copyOfRange(control, 1, CALLS);
        System.out.println("TIME osr-cold " + median(coldTail) / 1_000_000 + " ms/call");
        System.out.println("TIME osr-control " + median(controlTail) / 1_000_000 + " ms/call");
        System.out.println("TIME osr-cold-first " + cold[0] / 1_000_000 + " ms");
        System.out.println("check-cold " + sumCold);
        System.out.println("check-control " + sumControl);
    }
}
