// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 (hunt, verification): monitors held
// by a COMPILED frame when an exception leaves it, or when a handler inside
// the frame runs. HotSpot (JVMS 2.11.10, javac's exception-table monitorexit):
// * an exception escaping a synchronized block / method releases its monitor
//   (the caller's handler sees holdsLock == false, and another thread can
//   take the lock at once);
// * a handler INSIDE an outer synchronized block, catching what an inner
//   synchronized block threw, still holds the outer lock and not the inner;
// * after a deopt (a new receiver class after warm-up) the same holds.
// Every case runs cold once, then 30 000 times (compiled), then reports the
// hot answers; the output is the same in every tier.
//
// Run, each mode, JIT default, CRATONVM_BG_COMPILE=0, and --nojit:
//   cratonvm L2W101CompiledMonitorExceptionExit
//
// HOTSPOT_EXPECTED_BEGIN
// block-escape: held=false free=true
// method-escape: held=false free=true
// implicit-npe-escape: held=false free=true
// nested-handler: outer=true inner=false
// deopt-escape: held=false free=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.TimeUnit;

public class L2W101CompiledMonitorExceptionExit {
    static final Object A = new Object();
    static final Object INNER = new Object();
    static int counter;

    static void blockThrows(int i) {
        synchronized (A) {
            counter++;
            if (i < 0) {
                throw new IllegalStateException("x");
            }
        }
    }

    static synchronized void methodThrows(int i) {
        counter++;
        if (i < 0) {
            throw new IllegalStateException("y");
        }
    }

    static int npeInside(int[] a) {
        synchronized (A) {
            return a.length;
        }
    }

    static boolean[] nested(int i) {
        boolean[] r = new boolean[2];
        synchronized (A) {
            try {
                synchronized (INNER) {
                    if (i < 0) {
                        throw new IllegalStateException("z");
                    }
                }
            } catch (IllegalStateException e) {
                r[0] = Thread.holdsLock(A);
                r[1] = Thread.holdsLock(INNER);
            }
        }
        return r;
    }

    static class Base {
        void act(int i) {
            if (i < 0) {
                throw new IllegalStateException("b");
            }
        }
    }

    static class Late extends Base {
        @Override
        void act(int i) {
            if (i < 0) {
                throw new IllegalStateException("l");
            }
        }
    }

    static void deoptThrows(Base b, int i) {
        synchronized (A) {
            b.act(i);
        }
    }

    // Can another thread take `lock` within a second?
    static boolean freeForOthers(Object lock) throws InterruptedException {
        boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                got[0] = true;
            }
        });
        t.start();
        t.join(TimeUnit.SECONDS.toMillis(5));
        return got[0] && !t.isAlive();
    }

    interface Body {
        void run(int i);
    }

    static void escape(String name, Body body) throws InterruptedException {
        try {
            body.run(-1);
        } catch (RuntimeException e) {
            // cold
        }
        for (int i = 0; i < 30_000; i++) {
            body.run(i);
        }
        boolean held = true;
        try {
            body.run(-1);
        } catch (RuntimeException e) {
            held = Thread.holdsLock(A) || Thread.holdsLock(L2W101CompiledMonitorExceptionExit.class);
        }
        System.out.println(name + ": held=" + held + " free=" + (freeForOthers(A)
                && freeForOthers(L2W101CompiledMonitorExceptionExit.class)));
    }

    public static void main(String[] args) throws Exception {
        escape("block-escape", L2W101CompiledMonitorExceptionExit::blockThrows);
        escape("method-escape", L2W101CompiledMonitorExceptionExit::methodThrows);
        int[] arr = new int[3];
        escape("implicit-npe-escape", i -> npeInside(i < 0 ? null : arr));
        nested(-1);
        for (int i = 0; i < 30_000; i++) {
            nested(i);
        }
        boolean[] r = nested(-1);
        System.out.println("nested-handler: outer=" + r[0] + " inner=" + r[1]);
        Base base = new Base();
        Late late = new Late();
        escape("deopt-escape", i -> {
            if (i == 20_000) {
                deoptThrows(late, 1);
            }
            deoptThrows(i == 20_001 ? late : base, i);
        });
    }
}
