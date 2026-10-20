// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L7: the fused `getstatic #F; if<cond>`
// over a quickened int-typed static (`CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`,
// default off; proposal
// docs/internal/fixed-bugs/interpreter-i65-L7-proposal-a-fused-getstatic-and-if-zero-branch-for-static-flags-DONE-20261029.md).
// No behaviour change: every line is the same with the switch off and on
// (and with CRATONVM_INTERP_GETSTATIC_INLINE_HIT,
// CRATONVM_INTERP_PUTSTATIC_INLINE_HIT, CRATONVM_INTERP_HOISTED_OBSERVERS),
// with and without --nojit.
//
// Shapes: `if (flag)` true and false (ifeq), `if (!enabled) continue` with
// the flag cleared mid-loop (ifne), `while (running)` cleared by the body
// (the top test reads the new value), `do { } while (dflag)` (a BACKWARD
// fused branch: back-edge accounting, OSR, poll), an int static against zero
// under all six conditions at -3 / 0 / 5, a negative byte static, a char
// static, a class initialized by its first fused read (initializer prints
// first), a static read through a subclass (only the declaring class
// initializes), a volatile flag cleared by another thread (the spinning
// loop must see it), and a class whose initializer throws
// (ExceptionInInitializerError, then NoClassDefFoundError).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// ifzero flag-true 1000
// ifzero flag-false 0
// ifzero not-enabled 500
// ifzero while-running 500
// ifzero do-while 300
// ifzero level=-3 gt0=0 lt0=10 ge0=0 le0=10 ne0=10 eq0=0
// ifzero level=0 gt0=0 lt0=0 ge0=10 le0=10 ne0=0 eq0=10
// ifzero level=5 gt0=10 lt0=0 ge0=10 le0=0 ne0=10 eq0=0
// ifzero byte-neg 10
// ifzero char-nonzero 10
// init LazyFlag
// ifzero lazy 3
// init Base
// ifzero inherited 4
// ifzero volatile-stop true
// bad first java.lang.ExceptionInInitializerError
// bad second java.lang.NoClassDefFoundError
// HOTSPOT_EXPECTED_END
public class L7W66GetstaticIfZero {
    static boolean flag = true;
    static boolean off;
    static boolean enabled = true;
    static boolean running;
    static boolean dflag;
    static int level;
    static byte nb = -1;
    static char ch = 'q';
    static volatile boolean vflag;

    static class LazyFlag {
        static boolean on = true;

        static {
            System.out.println("init LazyFlag");
        }
    }

    static class Base {
        static int baseOn = 1;

        static {
            System.out.println("init Base");
        }
    }

    static class Sub extends Base {
        static {
            System.out.println("init Sub");
        }
    }

    static class BadFlag {
        static boolean on;

        static {
            if (alwaysTrue()) {
                throw new RuntimeException("boom");
            }
        }
    }

    static boolean alwaysTrue() {
        return true;
    }

    static int flagCount() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            if (flag) {
                n++;
            }
        }
        return n;
    }

    static int offCount() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            if (off) {
                n++;
            }
        }
        return n;
    }

    static int notEnabled() {
        int n = 0;
        for (int i = 0; i < 1000; i++) {
            if (i == 500) {
                enabled = false;
            }
            if (!enabled) {
                continue;
            }
            n++;
        }
        return n;
    }

    static int whileRunning() {
        running = true;
        int i = 0;
        while (running) {
            i++;
            if (i == 500) {
                running = false;
            }
        }
        return i;
    }

    static int doWhile() {
        dflag = true;
        int n = 0;
        do {
            n++;
            if (n == 300) {
                dflag = false;
            }
        } while (dflag);
        return n;
    }

    static String levels(int v) {
        level = v;
        int gt = 0;
        int lt = 0;
        int ge = 0;
        int le = 0;
        int ne = 0;
        int eq = 0;
        for (int i = 0; i < 10; i++) {
            if (level > 0) {
                gt++;
            }
        }
        for (int i = 0; i < 10; i++) {
            if (level < 0) {
                lt++;
            }
        }
        for (int i = 0; i < 10; i++) {
            if (level >= 0) {
                ge++;
            }
        }
        for (int i = 0; i < 10; i++) {
            if (level <= 0) {
                le++;
            }
        }
        for (int i = 0; i < 10; i++) {
            if (level != 0) {
                ne++;
            }
        }
        for (int i = 0; i < 10; i++) {
            if (level == 0) {
                eq++;
            }
        }
        return "level=" + v + " gt0=" + gt + " lt0=" + lt + " ge0=" + ge + " le0=" + le
                + " ne0=" + ne + " eq0=" + eq;
    }

    static int byteNeg() {
        int n = 0;
        for (int i = 0; i < 10; i++) {
            if (nb < 0) {
                n++;
            }
        }
        return n;
    }

    static int charNonzero() {
        int n = 0;
        for (int i = 0; i < 10; i++) {
            if (ch != 0) {
                n++;
            }
        }
        return n;
    }

    static int lazy() {
        int n = 0;
        for (int i = 0; i < 3; i++) {
            if (LazyFlag.on) {
                n++;
            }
        }
        return n;
    }

    static int inherited() {
        int n = 0;
        for (int i = 0; i < 4; i++) {
            if (Sub.baseOn != 0) {
                n++;
            }
        }
        return n;
    }

    static long spins;

    static void spin() {
        long s = 0;
        while (vflag) {
            s++;
        }
        spins = s;
    }

    static int badBody() {
        int n = 0;
        for (int i = 0; i < 3; i++) {
            if (BadFlag.on) {
                n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("ifzero flag-true " + flagCount());
        System.out.println("ifzero flag-false " + offCount());
        System.out.println("ifzero not-enabled " + notEnabled());
        System.out.println("ifzero while-running " + whileRunning());
        System.out.println("ifzero do-while " + doWhile());
        System.out.println("ifzero " + levels(-3));
        System.out.println("ifzero " + levels(0));
        System.out.println("ifzero " + levels(5));
        System.out.println("ifzero byte-neg " + byteNeg());
        System.out.println("ifzero char-nonzero " + charNonzero());
        int lz = lazy();
        System.out.println("ifzero lazy " + lz);
        int inh = inherited();
        System.out.println("ifzero inherited " + inh);
        vflag = true;
        Thread t = new Thread(L7W66GetstaticIfZero::spin);
        t.start();
        Thread.sleep(50);
        vflag = false;
        t.join();
        System.out.println("ifzero volatile-stop " + (spins >= 0));
        try {
            badBody();
            System.out.println("bad first no exception");
        } catch (Throwable e) {
            System.out.println("bad first " + e.getClass().getName());
        }
        try {
            badBody();
            System.out.println("bad second no exception");
        } catch (Throwable e) {
            System.out.println("bad second " + e.getClass().getName());
        }
    }
}
