// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 17, lane L5: the per-thread site caches after a
// retransform (docs/known-issues/interpreter/
// i14-L3-proposal-class-scoped-redefinition-invalidation-20260925.md,
// "Progress (wave 17)", stage 2a).
//
// A hot loop of instance field reads and writes, a static field read and a
// static call -- every one of them served by a per-thread site cache
// (`vm/src/runtime/interpreter/site_cache.rs`) once warm. It is timed, then
// ONE unrelated class (`Victim`) is retransformed with no transformer
// registered (the class bytes do not change), and the same loop is timed
// again.
//
//   * Before wave 17 the first redefinition anywhere latched every site cache
//     of every thread off for the rest of the process
//     (`any_class_redefined`), so the "after" phase paid the full slow path
//     on every field access and call: after/before well above 1 under
//     --nojit.
//   * Wave 17 (`SITE_CACHES_SURVIVE_REDEFINITION`): the redefinition retires
//     the old entries through the resolution epoch and the caches refill, so
//     after/before should return to ~1 under --nojit.
//   With the JIT on, the retransform still flushes every compiled body (the
//   full flush is unchanged this wave), so the "after" phase includes a
//   re-warm; compare --nojit first.
//
// SETUP (not runnable by the plain probe runner): the retransform needs an
// Instrumentation, i.e. a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineSiteCacheBench$Agent
//     Can-Retransform-Classes: true
// containing RedefineSiteCacheBench*.class, then run
//     cratonvm --compatible --nojit -javaagent:bench.jar -cp bench.jar RedefineSiteCacheBench [rounds]
// (and without --nojit); interleave with the previous build, 5 runs, medians
// (the microbenchmark-noise note). Without the agent the bench still runs,
// skips the retransform and says so on stderr.
//
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: ns per loop iteration before and after, and their ratio.
import java.lang.instrument.Instrumentation;

public class RedefineSiteCacheBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static final class Holder {
        static int k = 3;

        static int mix(int i) {
            return (i * 31) ^ k;
        }
    }

    static final class Hot {
        int a;
        int b;
        long c;

        long run(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                a += i;
                b ^= a;
                c += b;
                s += a + Holder.mix(b) + Holder.k;
            }
            return s + c;
        }
    }

    static final class Victim {
        int v() {
            return 7;
        }
    }

    static final int INNER = 20_000;

    static long phase(Hot hot, int reps) {
        long s = 0;
        for (int r = 0; r < reps; r++) {
            s += hot.run(INNER);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        Hot hot = new Hot();
        long checksum = 0;
        checksum += phase(hot, rounds / 4); // warm-up
        long t0 = System.nanoTime();
        checksum += phase(hot, rounds);
        long before = System.nanoTime() - t0;

        checksum += new Victim().v();
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported() && i.isModifiableClass(Victim.class)) {
            i.retransformClasses(Victim.class);
            System.err.println("retransformed " + Victim.class.getName());
        } else {
            System.err.println("no agent: the retransform was skipped (see the header)");
        }
        checksum += new Victim().v();

        checksum += phase(hot, rounds / 4); // re-warm
        long t1 = System.nanoTime();
        checksum += phase(hot, rounds);
        long after = System.nanoTime() - t1;

        System.out.println("checksum=" + checksum);
        double perBefore = (double) before / ((long) rounds * INNER);
        double perAfter = (double) after / ((long) rounds * INNER);
        System.err.printf("before=%.2f ns/iter after=%.2f ns/iter after/before=%.2f%n",
                perBefore, perAfter, perAfter / perBefore);
    }
}
