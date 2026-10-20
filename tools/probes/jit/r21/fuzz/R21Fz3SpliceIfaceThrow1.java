// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13, lane fuzz3: stack traces out of deep SPLICED chains
// of default methods, interface static methods, an interface private method
// and an `I.super.m()` call, thrown explicitly and implicitly at the leaf,
// before and after a class load makes the compiled callers not entrant.
//
// Risk targeted (waves 8-12): `r21-spliced-default-chain-trace-drops-the-root-
// frame` (a throw from a callee-deopt service lost the caller's spliced level;
// fixed by keeping the poisoned-bci chain), wave 9's "first not-entrant forward
// bumps the cache generation, walk reads generation first", wave 8's "trace walk
// keeps a not-entrant innermost frame decoded from its caller's call", and the
// StackWalker window fixes (empty window lists from 0). Every trace prints
// class-simple-name.method:line for every frame (the probe's frames only; the
// main thread's trace ends at main), the exception class and message, and the
// cause's trace for the wrapped rows. A StackWalker capture at the leaf
// (x == 21) prints the same, with and without a limit.
//
// Shape: row(B b, x) -> b.d6 -> d5 -> d4 (B overrides: private bp -> A.super.d4)
// -> d3 -> d2 -> d1 -> St.s1 -> St.s2 -> St.leaf. Leaf: x < 0 IAE, x == 7
// ArithmeticException (/ by zero), x == 13 AIOOBE, x == 21 walker capture.
// `wrap` rows go through an interface static that catches and rethrows
// IllegalStateException(cause). Passes: cold, hot (after 40k warm calls of the
// receivers K1 / K2), late (K3 loaded: overrides d2, so CHA-devirtualized
// d6..d1 chains in compiled callers go not entrant; K1 rows run first, then
// K3), rewarm, final.
//
// Run:
//   javac -d out R21Fz3SpliceIfaceThrow1.java
//   java -Xmx256m -cp out R21Fz3SpliceIfaceThrow1                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz3SpliceIfaceThrow1             # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz3SpliceIfaceThrow1
//   cratonvm --nojit -Xmx256m -cp out R21Fz3SpliceIfaceThrow1
//   CRATONVM_JIT_TRACE_NOT_ENTRANT_INNERMOST=0 cratonvm ...    # wave 8 switch off (control)
// HotSpot expectation: every line identical on every arm.

import java.util.List;
import java.util.stream.Collectors;

public class R21Fz3SpliceIfaceThrow1 {
    static final int WARM = 40_000;
    static volatile String captured = "";
    static volatile String capturedLimited = "";

    static String simple(String cn) {
        int i = Math.max(cn.lastIndexOf('$'), cn.lastIndexOf('.'));
        return cn.substring(i + 1);
    }

    static String frames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < st.length; i++) {
            if (!st[i].getClassName().startsWith("R21Fz3SpliceIfaceThrow1")) {
                continue;
            }
            if (sb.length() > 1) {
                sb.append(',');
            }
            sb.append(simple(st[i].getClassName())).append('.').append(st[i].getMethodName()).append(':')
                    .append(st[i].getLineNumber());
        }
        return sb.append(']').toString();
    }

    interface St {
        static int leaf(int x) {
            if (x < 0) {
                throw new IllegalArgumentException("neg" + x);
            }
            if (x == 7) {
                return 10 / (x - 7);
            }
            if (x == 13) {
                int[] a = new int[3];
                return a[x - 8];
            }
            if (x == 21) {
                List<String> all = StackWalker.getInstance().walk(s -> s
                        .filter(f -> f.getClassName().startsWith("R21Fz3SpliceIfaceThrow1"))
                        .map(f -> simple(f.getClassName()) + "." + f.getMethodName() + ":" + f.getLineNumber())
                        .collect(Collectors.toList()));
                captured = all.toString();
                capturedLimited = StackWalker.getInstance().walk(s -> s.limit(4)
                        .map(f -> simple(f.getClassName()) + "." + f.getMethodName())
                        .collect(Collectors.joining(",")));
            }
            return x;
        }

        static int s2(int x) {
            return leaf(x) + 2;
        }

        static int s1(int x) {
            return s2(x) * 3;
        }

        static int wrap(A a, int x) {
            try {
                return a.d6(x);
            } catch (RuntimeException e) {
                throw new IllegalStateException("wrapped " + e.getClass().getSimpleName(), e);
            }
        }
    }

    interface A {
        default int d1(int x) {
            return St.s1(x) + 1;
        }

        default int d2(int x) {
            return d1(x) + 1;
        }

        default int d3(int x) {
            return d2(x) + 1;
        }

        default int d4(int x) {
            return d3(x) + 1;
        }

        default int d5(int x) {
            return d4(x) + 1;
        }

        default int d6(int x) {
            return d5(x) + 1;
        }
    }

    interface B extends A {
        @Override
        default int d4(int x) {
            return bp(x) + 10;
        }

        private int bp(int x) {
            return A.super.d4(x);
        }
    }

    static final class K1 implements B {
    }

    static final class K2 implements A {
    }

    /** Never touched before the late pass: overriding d2 invalidates the d2 chains of B receivers. */
    static final class K3 implements B {
        @Override
        public int d2(int x) {
            return d1(x) + 100;
        }
    }

    static String row(String tag, B b, A a, int x) {
        StringBuilder sb = new StringBuilder(tag).append(" x=").append(x).append(' ');
        try {
            sb.append("b=").append(b.d6(x));
        } catch (RuntimeException e) {
            sb.append("b=").append(e.getClass().getName()).append(':').append(e.getMessage()).append(' ')
                    .append(frames(e.getStackTrace()));
        }
        sb.append(' ');
        try {
            sb.append("a=").append(a.d6(x));
        } catch (RuntimeException e) {
            sb.append("a=").append(e.getClass().getName()).append(':').append(e.getMessage()).append(' ')
                    .append(frames(e.getStackTrace()));
        }
        sb.append(' ');
        try {
            sb.append("w=").append(St.wrap(b, x));
        } catch (IllegalStateException e) {
            Throwable c = e.getCause();
            sb.append("w=").append(e.getMessage()).append(' ').append(frames(e.getStackTrace())).append(" cause=")
                    .append(c.getClass().getName()).append(':').append(c.getMessage()).append(' ')
                    .append(frames(c.getStackTrace()));
        }
        if (x == 21) {
            sb.append(" walk=").append(captured).append(" lim=").append(capturedLimited);
        }
        return sb.toString();
    }

    static final int[] XS = {3, -5, 7, 13, 21};

    static void pass(String tag, B b, A a) {
        for (int x : XS) {
            System.out.println(row(tag, b, a, x));
        }
    }

    static long warm(B b, A a, int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h = h * 31 + b.d6(i & 3) + a.d6(i & 3);
            if ((i & 63) == 0) {
                try {
                    h += b.d6(-1);
                } catch (IllegalArgumentException e) {
                    h += e.getMessage().length();
                }
                try {
                    h += St.wrap(a, 7);
                } catch (IllegalStateException e) {
                    h += e.getCause().getMessage().length();
                }
            }
        }
        return h;
    }

    public static void main(String[] args) {
        B k1 = new K1();
        A k2 = new K2();
        pass("cold", k1, k2);
        System.out.println("warm " + warm(k1, k2, WARM));
        pass("hot", k1, k2);
        B k3 = new K3();
        pass("late-k1", k1, k2);
        pass("late-k3", k3, k1);
        System.out.println("rewarm " + warm(k3, k1, WARM) + " " + warm(k1, k2, WARM / 4));
        pass("final-k1", k1, k3);
        pass("final-k3", k3, k2);
    }
}
