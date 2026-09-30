// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 18, lane L4: the interpreter's invoke fast doors
// and the compiled-code helper memos after ONE retransform of an unrelated
// class (docs/internal/fixed-bugs/
// interpreter-L5-fast-invoke-doors-decline-for-the-process-after-one-redefinition-FIXED-20260925.md,
// interpreter-L5-jit-helper-memos-latch-off-for-the-process-after-one-redefinition-FIXED-20260925.md).
//
// A hot loop of `invokestatic`, `invokespecial` (a private method and a super
// call), `invokevirtual`, an `ldc` of a String and a `checkcast`. It is timed,
// then ONE unrelated class (`Victim`) is retransformed with no transformer
// registered (its bytes do not change), and the same loop is timed again.
//
//   * Before wave 18 the first redefinition anywhere made every door decline
//     every call for the rest of the process (`any_class_redefined`), and the
//     JIT's `ldc` slot memo, typecheck answer memos and bytecode-callee
//     templates stopped serving: after/before well above 1 under --nojit
//     (the doors) and with the JIT on (the memos).
//   * Wave 18: a redefinition retires the entries it can change (the invoke
//     cache drops its maps once, the redefined class's `ldc` slots leave the
//     table) and everything else keeps serving, so after/before should be ~1
//     under --nojit. With the JIT on the retransform still flushes every
//     compiled body, so the "after" phase includes a re-warm (done before
//     timing); compare --nojit first, then the JIT on.
//   The door census tells the same story without a clock:
//   `CRATONVM_DBG=field-site` prints `door: static hit/miss ...` and the
//   decline reasons; "a class was redefined" must not appear.
//
// SETUP (not runnable by the plain probe runner): the retransform needs an
// Instrumentation, i.e. a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineDoorsBench$Agent
//     Can-Retransform-Classes: true
// containing RedefineDoorsBench*.class, then run
//     cratonvm --compatible --nojit -javaagent:bench.jar -cp bench.jar RedefineDoorsBench [rounds]
// (and without --nojit); interleave with the previous build, 5 runs, medians
// (the microbenchmark-noise note). Without the agent the bench still runs,
// skips the retransform and says so on stderr.
//
// stdout is deterministic: one "checksum=" line, identical on HotSpot 25.
// stderr: ns per loop iteration before and after, and their ratio.
import java.lang.instrument.Instrumentation;

public class RedefineDoorsBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static class Base {
        int base(int x) {
            return x + 1;
        }
    }

    static final class Hot extends Base {
        int acc;
        Object boxed = "seed";

        static int sq(int x) {
            return x * x;
        }

        private int priv(int x) {
            return x ^ 5;
        }

        int virt(int x) {
            return x - 3;
        }

        @Override
        int base(int x) {
            return super.base(x) + 2;
        }

        long run(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += sq(i & 15);
                s += priv(i);
                s += virt(i);
                s += base(i);
                String lit = "doors";
                s += lit.length();
                s += ((String) boxed).length();
            }
            return s;
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
