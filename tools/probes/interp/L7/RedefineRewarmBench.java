// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 20, lane L2: the invoke-cache re-warm a
// retransform of an UNRELATED class costs (docs/internal/fixed-bugs/
// interpreter-L4-proposal-class-scoped-invoke-cache-retirement-FIXED-20260926.md,
// "Progress (wave 20)").
//
// `Sites.sweep` holds 32 warm `invokestatic` sites and 8 warm
// `invokevirtual` sites. Each round retransforms `Victim` (no transformer is
// registered, so its bytes do not change) and then times ONE sweep: the first
// call through every site after the redefinition. The same sweep is also
// timed in rounds with no retransform, as the steady-state baseline.
//
//   * Wave 18/19: every redefinition dropped every thread's whole invoke
//     cache on its next lookup, so the first sweep after it re-resolved all
//     40 sites on the slow path: first/steady well above 1 under --nojit.
//   * Wave 20 (`INVOKE_CACHE_RETIRES_ONLY_REDEFINED_CALLERS`): the lookup
//     reads the redefinition ring and drops only `Victim`'s own caller
//     entries, so first/steady should fall toward 1 under --nojit (the site
//     caches still retire through the VM's resolution epoch, which is a
//     separate, per-VM cost). The census says the same without a clock:
//     `CRATONVM_INVOKE_CACHE_STATS=1` prints
//     `[cratonvm] invoke cache redefinition retirements: class-scoped=N ...
//     full=M`, and M should be ~0.
//
// SETUP (not runnable by the plain probe runner): the retransform needs an
// Instrumentation, i.e. a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineRewarmBench$Agent
//     Can-Retransform-Classes: true
// containing RedefineRewarmBench*.class, then run
//     cratonvm --compatible --nojit -javaagent:bench.jar -cp bench.jar RedefineRewarmBench [rounds]
// interleaved with the previous build, 5 runs, medians (the
// microbenchmark-noise note). Without the agent the bench still runs, skips
// the retransforms and says so on stderr.
//
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: ns per first sweep after a retransform, ns per steady sweep, and
// their ratio.
import java.lang.instrument.Instrumentation;

public class RedefineRewarmBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static final class T {
        static int t0(int x) { return x + 0; }
        static int t1(int x) { return x + 1; }
        static int t2(int x) { return x + 2; }
        static int t3(int x) { return x + 3; }
        static int t4(int x) { return x + 4; }
        static int t5(int x) { return x + 5; }
        static int t6(int x) { return x + 6; }
        static int t7(int x) { return x + 7; }
        static int t8(int x) { return x ^ 8; }
        static int t9(int x) { return x ^ 9; }
        static int t10(int x) { return x ^ 10; }
        static int t11(int x) { return x ^ 11; }
        static int t12(int x) { return x ^ 12; }
        static int t13(int x) { return x ^ 13; }
        static int t14(int x) { return x ^ 14; }
        static int t15(int x) { return x ^ 15; }
        static int t16(int x) { return x * 3 + 16; }
        static int t17(int x) { return x * 3 + 17; }
        static int t18(int x) { return x * 3 + 18; }
        static int t19(int x) { return x * 3 + 19; }
        static int t20(int x) { return x * 3 + 20; }
        static int t21(int x) { return x * 3 + 21; }
        static int t22(int x) { return x * 3 + 22; }
        static int t23(int x) { return x * 3 + 23; }
        static int t24(int x) { return (x >>> 1) + 24; }
        static int t25(int x) { return (x >>> 1) + 25; }
        static int t26(int x) { return (x >>> 1) + 26; }
        static int t27(int x) { return (x >>> 1) + 27; }
        static int t28(int x) { return (x >>> 1) + 28; }
        static int t29(int x) { return (x >>> 1) + 29; }
        static int t30(int x) { return (x >>> 1) + 30; }
        static int t31(int x) { return (x >>> 1) + 31; }
    }

    static class V {
        int a(int x) { return x + 100; }
        int b(int x) { return x + 101; }
        int c(int x) { return x + 102; }
        int d(int x) { return x + 103; }
        int e(int x) { return x + 104; }
        int f(int x) { return x + 105; }
        int g(int x) { return x + 106; }
        int h(int x) { return x + 107; }
    }

    static final class Sites {
        static long sweep(V v, int x) {
            long s = 0;
            s += T.t0(x); s += T.t1(x); s += T.t2(x); s += T.t3(x);
            s += T.t4(x); s += T.t5(x); s += T.t6(x); s += T.t7(x);
            s += T.t8(x); s += T.t9(x); s += T.t10(x); s += T.t11(x);
            s += T.t12(x); s += T.t13(x); s += T.t14(x); s += T.t15(x);
            s += T.t16(x); s += T.t17(x); s += T.t18(x); s += T.t19(x);
            s += T.t20(x); s += T.t21(x); s += T.t22(x); s += T.t23(x);
            s += T.t24(x); s += T.t25(x); s += T.t26(x); s += T.t27(x);
            s += T.t28(x); s += T.t29(x); s += T.t30(x); s += T.t31(x);
            s += v.a(x); s += v.b(x); s += v.c(x); s += v.d(x);
            s += v.e(x); s += v.f(x); s += v.g(x); s += v.h(x);
            return s;
        }
    }

    static final class Victim {
        int v() {
            return 7;
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        V v = new V();
        long checksum = 0;
        for (int i = 0; i < 20_000; i++) {
            checksum += Sites.sweep(v, i); // warm every site
        }
        checksum += new Victim().v();

        Instrumentation inst0 = inst;
        boolean agent = inst0 != null && inst0.isRetransformClassesSupported()
                && inst0.isModifiableClass(Victim.class);
        if (!agent) {
            System.err.println("no agent: the retransforms were skipped (see the header)");
        }
        long firstNs = 0;
        long steadyNs = 0;
        for (int r = 0; r < rounds; r++) {
            if (agent) {
                inst0.retransformClasses(Victim.class);
            }
            long t0 = System.nanoTime();
            checksum += Sites.sweep(v, r);
            firstNs += System.nanoTime() - t0;
            // Steady: the same sweep, no redefinition in between.
            long t1 = System.nanoTime();
            checksum += Sites.sweep(v, r + 1);
            steadyNs += System.nanoTime() - t1;
        }
        checksum += new Victim().v();

        System.out.println("checksum=" + checksum);
        double first = (double) firstNs / rounds;
        double steady = (double) steadyNs / rounds;
        System.err.printf("first sweep after a retransform=%.0f ns steady sweep=%.0f ns first/steady=%.2f%n",
                first, steady, first / steady);
    }
}
