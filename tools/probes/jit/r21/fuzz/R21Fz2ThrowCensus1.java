// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: THROWS21-1, the compiled-catch census
// skipped (`CRATONVM_JIT_THROW_CENSUS_STATS_ONLY`, opt-in;
// `helpers::note_local_handler_at_catch` -> `ThrowableLayoutCache::throw_census_skipped`,
// latched once per VM) on every arm of `jit_local_handler_lookup_body` and
// `local_handler_enter_implicit` (entered / propagated / implicit-entered).
//
// Risk targeted: the switch must change nothing but the stats line. The
// edits sit on the exact paths that decide entered-vs-propagated, so a
// misplaced early return would propagate an exception a compiled handler
// should catch, or swallow one. Four threads run catch loops at once
// (true sharing was the cost): implicit NPE / AIOOBE / ArithmeticException /
// ClassCastException / NegativeArraySizeException caught in the same method;
// an explicit throw caught locally; a throw propagated one and two frames up;
// a handler that does not match (propagated) inside one that does; finally
// blocks; a rethrow; a caught exception's message and stack top. Exact
// per-thread counts. Then a deopt trigger (a new exception subclass through
// the same handlers) and again.
//
// Run:
//   javac -d out R21Fz2ThrowCensus1.java
//   java -Xmx256m -cp out R21Fz2ThrowCensus1                             # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2ThrowCensus1
//   CRATONVM_JIT_THROW_CENSUS_STATS_ONLY=1 cratonvm -Xmx256m -cp out R21Fz2ThrowCensus1
//   CRATONVM_JIT_THROW_CENSUS_STATS_ONLY=1 CRATONVM_DBG_JIT_METHOD_STATS=1 cratonvm ... (stderr ignored)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every stdout line identical on every arm.

public class R21Fz2ThrowCensus1 {
    static final int PER_THREAD = 15_000; // x4 threads: each method >= 60k calls

    static class MyEx extends RuntimeException {
        MyEx(String m) {
            super(m);
        }
    }

    static class LateEx extends MyEx {
        LateEx(String m) {
            super(m);
        }
    }

    static volatile boolean useLate;

    static int implicitKinds(int i, int[] arr, Object o) {
        int k = i % 6;
        try {
            switch (k) {
                case 0:
                    return ((String) null).length();
                case 1:
                    return arr[i & 15];
                case 2:
                    return 10 / (i & 0);
                case 3:
                    return ((Integer) o).intValue();
                case 4:
                    return new int[-(i & 7) - 1].length;
                default:
                    return 1;
            }
        } catch (NullPointerException e) {
            return 100;
        } catch (ArrayIndexOutOfBoundsException e) {
            return 200;
        } catch (ArithmeticException e) {
            return 300;
        } catch (ClassCastException e) {
            return 400;
        } catch (NegativeArraySizeException e) {
            return 500;
        }
    }

    static void thrower(int i) {
        if ((i & 3) != 0) {
            throw useLate && (i & 7) == 1 ? new LateEx("late" + (i & 7)) : new MyEx("m" + (i & 7));
        }
    }

    static int oneUp(int i) {
        thrower(i);
        return 1;
    }

    static int twoUp(int i) {
        return oneUp(i) + 1;
    }

    static int explicit(int i) {
        int r = 0;
        try {
            r += twoUp(i);
        } catch (MyEx e) {
            r += e.getMessage().length() * 10;
        }
        try {
            try {
                thrower(i);
                r += 1000;
            } catch (IllegalStateException wrong) {
                r -= 99_999;
            } finally {
                r += 7;
            }
        } catch (MyEx e) {
            r += e instanceof LateEx ? 3 : 2;
        }
        return r;
    }

    static int rethrow(int i) {
        try {
            try {
                thrower(i);
                return 1;
            } catch (MyEx e) {
                throw new IllegalStateException("wrapped", e);
            }
        } catch (IllegalStateException e) {
            return e.getCause().getMessage().hashCode() & 0xFF;
        }
    }

    static long run(int seed) {
        int[] arr = new int[8];
        Object o = "notInt";
        long s = 0;
        for (int i = 0; i < PER_THREAD; i++) {
            int v = i + seed;
            s += implicitKinds(v, arr, o) + explicit(v) + rethrow(v);
        }
        return s;
    }

    static long[] parallel(int threads) throws InterruptedException {
        long[] out = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> out[id] = run(id * 1000));
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        return out;
    }

    static String top(Throwable t) {
        StackTraceElement e = t.getStackTrace()[0];
        return t.getClass().getSimpleName() + ":" + t.getMessage() + "@" + e.getMethodName();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("single cold " + run(0));
        long[] a = parallel(4);
        System.out.println("parallel " + a[0] + " " + a[1] + " " + a[2] + " " + a[3]);
        try {
            twoUp(1);
        } catch (MyEx e) {
            System.out.println("top " + top(e));
        }
        // Deopt trigger: a new exception subclass through the same handlers.
        useLate = true;
        long[] b = parallel(4);
        System.out.println("parallel late " + b[0] + " " + b[1] + " " + b[2] + " " + b[3]);
        try {
            twoUp(1);
        } catch (MyEx e) {
            System.out.println("top late " + top(e));
        }
        useLate = false;
        System.out.println("single after " + run(0));
    }
}
