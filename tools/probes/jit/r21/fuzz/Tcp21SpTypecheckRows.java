// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT tier2 wave 3, lane tcperf (tag tcp21, TCPERF21-4): single-pass
// `instanceof` / `checkcast` against a class at depth 1, a class at depth 2
// and an interface, over a mixed receiver pool. Run with the optimizing tier
// refused (CRATONVM_C2_ACCEPT=never) so the single-pass body runs, and compare
// CRATONVM_JIT_SP_TYPECHECK_ROW_FIRST=1 (the receiver's type-check row first)
// with the default (display arm only).
//
// Expected correctness lines are DERIVED from JLS 15.20.2 / 5.5 and the pool
// below, NOT measured (no JDK 25 HotSpot on the authoring host). Per pass over
// the 8 pool objects
//   { A1, A2, B1, Base, Other, "str", new int[1], new Base[1] }
// where A1 extends Base implements Shape, A2 extends A1 implements Named,
// B1 extends Base implements Shape, Other is unrelated:
//   instanceof Base  -> A1, A2, B1, Base            = 4
//   instanceof A1    -> A1, A2                      = 2
//   instanceof Shape -> A1, A2, B1                  = 3
//   instanceof Named -> A2                          = 1
//   instanceof Other -> Other                       = 1
//   (Base) cast succeeds for 4, throws ClassCastException for 4
//   (Shape) cast succeeds for 3, throws ClassCastException for 5
// With REPS passes each count is multiplied by REPS.
//
// Run:
//   javac -d out Tcp21SpTypecheckRows.java
//   java -Xmx256m -cp out Tcp21SpTypecheckRows                                   # HotSpot 25 (when available) / JDK 21
//   CRATONVM_C2_ACCEPT=never cratonvm -Xmx256m -cp out Tcp21SpTypecheckRows
//   CRATONVM_C2_ACCEPT=never CRATONVM_JIT_SP_TYPECHECK_ROW_FIRST=1 cratonvm -Xmx256m -cp out Tcp21SpTypecheckRows
//   (the same two with --compatible)
// PASS: every `fact` line reads OK on every arm and in both modes. The `time`
// lines are the A/B (lower is better); they are not asserted.

public class Tcp21SpTypecheckRows {
    static final int REPS = 20_000;

    interface Shape {
    }

    interface Named {
    }

    static class Base {
    }

    static class A1 extends Base implements Shape {
    }

    static class A2 extends A1 implements Named {
    }

    static class B1 extends Base implements Shape {
    }

    static class Other {
    }

    static Object[] pool() {
        return new Object[] {
            new A1(), new A2(), new B1(), new Base(), new Other(), "str", new int[1], new Base[1]
        };
    }

    static long kBase(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof Base) {
                    n++;
                }
            }
        }
        return n;
    }

    static long kA1(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof A1) {
                    n++;
                }
            }
        }
        return n;
    }

    static long kShape(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof Shape) {
                    n++;
                }
            }
        }
        return n;
    }

    static long kNamed(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof Named) {
                    n++;
                }
            }
        }
        return n;
    }

    static long kOther(Object[] a, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof Other) {
                    n++;
                }
            }
        }
        return n;
    }

    /** Successful casts to Base, failures counted apart (JLS 5.5). */
    static long[] kCastBase(Object[] a, int reps) {
        long ok = 0;
        long cce = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                try {
                    Base b = (Base) a[i];
                    if (b != null) {
                        ok++;
                    }
                } catch (ClassCastException e) {
                    cce++;
                }
            }
        }
        return new long[] {ok, cce};
    }

    static long[] kCastShape(Object[] a, int reps) {
        long ok = 0;
        long cce = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                try {
                    Shape s = (Shape) a[i];
                    if (s != null) {
                        ok++;
                    }
                } catch (ClassCastException e) {
                    cce++;
                }
            }
        }
        return new long[] {ok, cce};
    }

    static boolean failed;

    static void fact(String name, long expected, long got) {
        boolean ok = expected == got;
        if (!ok) {
            failed = true;
        }
        System.out.println("fact " + name + " expected=" + expected + " got=" + got + " " + (ok ? "OK" : "FAIL"));
    }

    static void time(String name, long nanos, long checks) {
        System.out.println("time " + name + " " + (nanos / Math.max(1, checks)) + " ns/check");
    }

    public static void main(String[] args) {
        Object[] a = pool();
        long checks = (long) REPS * a.length;
        // Warm every kernel so the single-pass body (or whatever tier runs it)
        // is the one measured, then count.
        for (int w = 0; w < 3; w++) {
            kBase(a, REPS);
            kA1(a, REPS);
            kShape(a, REPS);
            kNamed(a, REPS);
            kOther(a, REPS);
            kCastBase(a, REPS / 4);
            kCastShape(a, REPS / 4);
        }
        long t0 = System.nanoTime();
        long base = kBase(a, REPS);
        long t1 = System.nanoTime();
        long a1 = kA1(a, REPS);
        long t2 = System.nanoTime();
        long shape = kShape(a, REPS);
        long t3 = System.nanoTime();
        long named = kNamed(a, REPS);
        long t4 = System.nanoTime();
        long other = kOther(a, REPS);
        long t5 = System.nanoTime();
        long[] cb = kCastBase(a, REPS / 4);
        long t6 = System.nanoTime();
        long[] cs = kCastShape(a, REPS / 4);
        long t7 = System.nanoTime();
        fact("instanceof-Base", 4L * REPS, base);
        fact("instanceof-A1", 2L * REPS, a1);
        fact("instanceof-Shape", 3L * REPS, shape);
        fact("instanceof-Named", 1L * REPS, named);
        fact("instanceof-Other", 1L * REPS, other);
        fact("cast-Base-ok", 4L * (REPS / 4), cb[0]);
        fact("cast-Base-cce", 4L * (REPS / 4), cb[1]);
        fact("cast-Shape-ok", 3L * (REPS / 4), cs[0]);
        fact("cast-Shape-cce", 5L * (REPS / 4), cs[1]);
        time("instanceof-Base", t1 - t0, checks);
        time("instanceof-A1", t2 - t1, checks);
        time("instanceof-Shape", t3 - t2, checks);
        time("instanceof-Named", t4 - t3, checks);
        time("instanceof-Other", t5 - t4, checks);
        time("cast-Base", t6 - t5, checks / 4);
        time("cast-Shape", t7 - t6, checks / 4);
        System.out.println(failed ? "RESULT FAIL" : "RESULT OK");
        if (failed) {
            System.exit(1);
        }
    }
}
