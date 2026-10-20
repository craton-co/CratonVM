// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Shape 2 (b) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md:
// an optimizing back-edge poll whose loop header names an object escape
// analysis scalar-replaced -- a for-each iterator live across the loop, the
// shape of javac's `Code.compressCatchTable` -- had no exit
// (`not_current`), so the running activation finished compiled after an
// agent's retransform withdrew its body. HotSpot deoptimizes it,
// re-materializing the iterator, and the loop reads the new value at once.
//
// `walk` iterates a `Range` whose iterator the optimizing tier scalar-replaces,
// reading a spliced `Callee.value()` per element, and stops at the first
// element that sees a value other than the one it started with. An agent
// retransforms `Callee` 300 ms into the call. Rows:
//   left early   the loop saw the new value before the end of the range
//   consistent   the element it stopped at is the one its count says (a
//                re-materialized iterator whose cursor were stale would skip
//                or repeat one)
//
// HotSpot 25.0.3 (agent; default flags and -Xint):
//   round 1 left early=true consistent=true
//   round 2 left early=true consistent=true
//
// SETUP: a jar whose manifest has
//     Premain-Class: I41ScalarIteratorLoopLeaves$Agent
//     Can-Retransform-Classes: true
// containing I41ScalarIteratorLoopLeaves*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar I41ScalarIteratorLoopLeaves
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;

public class I41ScalarIteratorLoopLeaves {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        static int value() {
            return 12345;
        }
    }

    /** Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th transform. */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"I41ScalarIteratorLoopLeaves$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    /** A range of ints with a trivial iterator, which escape analysis replaces. */
    static final class Range {
        final long limit;

        Range(long limit) {
            this.limit = limit;
        }

        It iterator() {
            return new It(limit);
        }
    }

    static final class It {
        long cursor;
        final long limit;

        It(long limit) {
            this.limit = limit;
        }

        boolean hasNext() {
            return cursor < limit;
        }

        long next() {
            return cursor++;
        }
    }

    /**
     * Walks `range` until an element sees a `Callee.value()` other than
     * `before`. Answers `{count, element}` of the element it stopped at (or of
     * the last one).
     */
    public static long[] walk(Range range, int before) {
        long count = 0;
        long last = -1;
        for (It it = range.iterator(); it.hasNext(); ) {
            long e = it.next();
            count++;
            last = e;
            sink = e;
            if (Callee.value() != before) {
                break;
            }
        }
        return new long[] {count, last};
    }

    /** Written per element, so no compiler may compute the walk in closed form. */
    static volatile long sink;

    /** Far more elements than any compiled loop walks in a second. */
    static final long LIMIT = 1L << 50;

    static long[] call(Method m, Range r, int before) throws Exception {
        return (long[]) m.invoke(null, r, before);
    }

    static void round(int n, Method m) throws Exception {
        Range small = new Range(500);
        for (int k = 0; k < 20_000; k++) {
            call(m, small, Callee.value());
        }
        Thread.sleep(500);
        int before = Callee.value();
        long[][] result = new long[1][];
        Thread worker = new Thread(() -> {
            try {
                result[0] = call(m, new Range(LIMIT), before);
            } catch (Exception e) {
                result[0] = null;
            }
        }, "walker");
        worker.setDaemon(true);
        worker.start();
        Thread.sleep(300);
        inst.retransformClasses(Callee.class);
        worker.join(120_000);
        long[] r = result[0];
        System.out.println("round " + n + " left early=" + (r != null && r[0] < LIMIT)
                + " consistent=" + (r != null && r[1] == r[0] - 1));
    }

    public static void main(String[] args) throws Exception {
        if (inst == null || !inst.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        inst.addTransformer(new Bump(), true);
        Method m = I41ScalarIteratorLoopLeaves.class.getMethod("walk", Range.class, int.class);
        round(1, m);
        round(2, m);
    }
}
