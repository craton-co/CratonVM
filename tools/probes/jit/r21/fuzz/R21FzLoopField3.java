// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: transfers out of a promoted receiver-field
// loop (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY`; the promotion's
// "NO TRANSFER INSIDE THE LOOP" rule, the conditional write-back, and the
// poll exit refusal at a promoted header).
//
// Risk targeted: while a cell is promoted, memory is stale between the first
// deleted store and the exit. Every way out of the loop must therefore either
// be refused by the census or see every completed iteration:
//   * an ArithmeticException (`/ (i - bad)`) mid-loop, caught by the CALLER:
//     `this.f` must hold the iterations before the throw;
//   * the same with a handler INSIDE the method (the exception edge stays in
//     the method);
//   * an AIOOBE from an array read in the loop;
//   * a break exit (`if (f > lim) break`) and a return from inside the loop;
//   * a synchronized method and a volatile sibling field (never promoted);
//   * a `this` stored into a static before the loop and read by a callee
//     invoked after the loop (the write-back must precede the call).
// Rows cold, hot, and after a deopt trigger (a late subclass).
//
// Run:
//   javac -d out R21FzLoopField3.java
//   java -Xmx256m -cp out R21FzLoopField3                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopField3
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm -Xmx256m -cp out R21FzLoopField3
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopField3 {
    static final int WARM = 60_000;
    static R21FzLoopField3 published;

    int f;
    long g;
    volatile int v;
    int[] data = {1, 2, 3, 4, 5, 6, 7, 8};

    int divLoop(int n, int bad) {
        for (int i = 0; i < n; i++) {
            f += 1000 / (i - bad);
        }
        return f;
    }

    int divLoopCaught(int n, int bad) {
        try {
            for (int i = 0; i < n; i++) {
                f += 1000 / (i - bad);
            }
        } catch (ArithmeticException e) {
            return -f;
        }
        return f;
    }

    long arrLoop(int n) {
        for (int i = 0; i < n; i++) {
            g += data[i];
        }
        return g;
    }

    int breakLoop(int n, int lim) {
        for (int i = 0; i < n; i++) {
            f += i;
            if (f > lim) {
                break;
            }
        }
        return f;
    }

    int returnLoop(int n, int lim) {
        for (int i = 0; i < n; i++) {
            f += 3;
            if (i == lim) {
                return f * 10;
            }
        }
        return f;
    }

    synchronized int syncLoop(int n) {
        for (int i = 0; i < n; i++) {
            f += i;
        }
        return f;
    }

    int volatileSibling(int n) {
        for (int i = 0; i < n; i++) {
            f += i;
            v = f;
        }
        return v;
    }

    static int readPublished() {
        return published.f;
    }

    int publishThenCall(int n) {
        published = this;
        for (int i = 0; i < n; i++) {
            f += i * 2;
        }
        return readPublished();
    }

    int step(int i) {
        return i;
    }

    int virtualStep(int n) {
        for (int i = 0; i < n; i++) {
            f += step(i);
        }
        return f;
    }

    static final class Late extends R21FzLoopField3 {
        @Override
        int step(int i) {
            if (i == 7) {
                throw new IllegalStateException("late step " + f);
            }
            return -i;
        }
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void rows(String tag, R21FzLoopField3 o) {
        o.f = 0;
        try {
            o.divLoop(20, 9);
            System.out.println("row " + tag + " div no throw " + o.f);
        } catch (ArithmeticException e) {
            System.out.println("row " + tag + " div " + exc(e) + " f=" + o.f);
        }
        o.f = 0;
        System.out.println("row " + tag + " divCaught " + o.divLoopCaught(20, 13) + " f=" + o.f);
        o.f = 0;
        System.out.println("row " + tag + " divNone " + o.divLoop(20, -1) + " f=" + o.f);
        o.g = 0;
        try {
            o.arrLoop(12);
            System.out.println("row " + tag + " arr no throw " + o.g);
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("row " + tag + " arr " + exc(e) + " g=" + o.g);
        }
        o.f = 0;
        System.out.println("row " + tag + " break " + o.breakLoop(100, 50) + " f=" + o.f);
        o.f = 0;
        System.out.println("row " + tag + " return " + o.returnLoop(100, 6) + " f=" + o.f);
        o.f = 0;
        System.out.println("row " + tag + " sync " + o.syncLoop(30) + " f=" + o.f);
        o.f = 0;
        System.out.println("row " + tag + " volatile " + o.volatileSibling(30) + " f=" + o.f);
        o.f = 0;
        System.out.println("row " + tag + " publish " + o.publishThenCall(30) + " f=" + o.f);
        o.f = 0;
        try {
            System.out.println("row " + tag + " virtual " + o.virtualStep(10) + " f=" + o.f);
        } catch (IllegalStateException e) {
            System.out.println("row " + tag + " virtual " + exc(e) + " f=" + o.f);
        }
    }

    public static void main(String[] args) {
        R21FzLoopField3 o = new R21FzLoopField3();
        rows("cold", o);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int n = 4 + (r & 3);
            h += o.divLoop(n, -1);
            h += o.divLoopCaught(n, -2);
            o.g = 0;
            h += o.arrLoop(n);
            h += o.breakLoop(n, 1 << 30);
            h += o.returnLoop(n, 99);
            h += o.syncLoop(n);
            h += o.volatileSibling(n);
            h += o.publishThenCall(n);
            h += o.virtualStep(n);
            h = h * 31 + o.f;
        }
        System.out.println("warm hash " + h + " f=" + o.f);
        rows("hot", o);
        R21FzLoopField3 late = new Late();
        rows("late", late);
        rows("after", o);
    }
}
