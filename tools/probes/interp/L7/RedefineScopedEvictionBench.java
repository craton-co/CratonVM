// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 20, lane L1: what a burst of retransforms of an
// UNRELATED class costs the hot compiled code of the rest of the program
// (stage 2b of docs/known-issues/interpreter/i14-L3-proposal-class-scoped-redefinition-invalidation-20260925.md).
//
// Each round retransforms `Victim` (no transformer is registered, so its
// bytes do not change; Mockito's inline mock maker and coverage agents
// retransform in bursts like this) and then IMMEDIATELY times a slice of the
// hot loop, with no re-warm phase in between: the slice pays whatever the
// retransform took from the compiled code.
//
//   * Before wave 20 every retransform flushed the VM's whole code cache, so
//     each slice ran interpreted until the loop recompiled -- and a method
//     already at C2 was never offered for compilation again (its tier was not
//     reset), so the slices stayed slow: after/before well above 1.
//   * Wave 20: `Victim` was never inlined anywhere, so the retransform
//     withdraws only Victim's own bodies and `Hot.run` keeps its compiled
//     body: after/before ~1. Compare with the JIT on (the stage changes
//     nothing under --nojit), interleaved with the previous build, 5 runs,
//     medians. `CRATONVM_DBG=jit-method-stats` shows the eviction count on
//     its `redefine-evicted-bodies=` field.
//
// SETUP (not runnable by the plain probe runner): the retransform needs an
// Instrumentation, i.e. a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineScopedEvictionBench$Agent
//     Can-Retransform-Classes: true
// containing RedefineScopedEvictionBench*.class, then run
//     cratonvm --compatible -javaagent:bench.jar -cp bench.jar RedefineScopedEvictionBench [rounds]
// Without the agent the bench still runs, skips the retransforms and says so
// on stderr.
//
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: ns per loop iteration before and after, and their ratio.
import java.lang.instrument.Instrumentation;

public class RedefineScopedEvictionBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static final class Hot {
        static int mix(int x) {
            return (x * 31) ^ (x >>> 3);
        }

        static long run(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += mix(i);
                s ^= s << 1;
            }
            return s;
        }
    }

    static final class Victim {
        // A field initializer: the constructor is not the empty one a compiled
        // caller may elide, so no compile copies Victim's bytecode.
        int seed = 7;

        int v() {
            return seed;
        }
    }

    /**
     * Uses Victim outside every hot method. The loop keeps this method from
     * being spliced into a compiled `main` (an OSR body of main would
     * otherwise copy `v()`, and a class some compile copied takes the full
     * flush).
     */
    static int touchVictim() {
        int s = 0;
        for (int k = 0; k < 1; k++) {
            s += new Victim().v();
        }
        return s;
    }

    static final int SLICE = 200_000;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        long checksum = 0;
        for (int w = 0; w < 200; w++) {
            checksum += Hot.run(SLICE);
        }
        checksum += touchVictim();

        long before = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            checksum += Hot.run(SLICE);
            before += System.nanoTime() - t0;
        }

        Instrumentation i = inst;
        boolean retransform = i != null && i.isRetransformClassesSupported()
                && i.isModifiableClass(Victim.class);
        if (!retransform) {
            System.err.println("no agent: the retransforms were skipped (see the header)");
        }
        long after = 0;
        for (int r = 0; r < rounds; r++) {
            if (retransform) {
                i.retransformClasses(Victim.class);
            }
            long t0 = System.nanoTime();
            checksum += Hot.run(SLICE);
            after += System.nanoTime() - t0;
        }
        checksum += touchVictim();

        System.out.println("checksum=" + checksum);
        double perBefore = (double) before / ((long) rounds * SLICE);
        double perAfter = (double) after / ((long) rounds * SLICE);
        System.err.printf("before=%.2f ns/iter after=%.2f ns/iter after/before=%.2f%n",
                perBefore, perAfter, perAfter / perBefore);
    }
}
