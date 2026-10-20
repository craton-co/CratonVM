// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the keep set at a spliced callee's loop
// exit (`CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS`, default ON, `=0` restores
// the pre-fix liveness; `CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS`,
// default ON), WITHOUT a debugger.
//
// Risk targeted: R21KeepsetSplicedLoopLocal needs JDWP to send the compiled
// loop to the interpreter. Any other mode exit at the spliced loop's poll
// does the same: here a CHA dependency of the compiled caller (the callee's
// loop calls `s.v()` on an abstract type with ONE loaded implementation) is
// broken by ANOTHER thread loading a second implementation while the loop
// runs. If the rebuilt callee frame lost a local that the loop reads only
// through a hoisted invariant compare (`b == t`), the interpreter resumes
// with `b == null`, and the read AFTER the loop (`b.v`) throws an NPE or the
// compare flips. The answers do not depend on when the class is loaded:
// every rep prints the same sum on HotSpot. Second kernel: a post-call
// chain read (a local kept only for a read after a non-inlined call in the
// loop).
//
// Run:
//   javac -d out R21FzKeepSplice1.java
//   java -Xmx256m -cp out R21FzKeepSplice1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzKeepSplice1
//   CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS=0 cratonvm -Xmx256m -cp out R21FzKeepSplice1 (pre-fix control)
//   CRATONVM_JIT_IR_KEEP_SET_MODELLED_CHAIN_READS=0 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ... (slow: the loops are long)
// Engagement: CRATONVM_DBG_IR_SLOTS=1 `[ir-slots] spliced latch exit reads`, CRATONVM_DBG_DEOPT=1 for the exit.
// HotSpot expectation: every line identical on every arm.

public class R21FzKeepSplice1 {
    static final int N = 40_000_000;
    static volatile int phase;

    static final class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    abstract static class Shape {
        abstract int v();
    }

    static final class One extends Shape {
        int v() {
            return 1;
        }
    }

    static final class Late extends Shape {
        int v() {
            return 2;
        }
    }

    /** The callee, small enough to be spliced into drive(). */
    static long sum(Box b, Box t, int n, Shape s) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            if (b == t) {
                acc += i & 3;
            } else {
                acc -= 1;
            }
            acc += s.v();
        }
        return acc + b.v * 1000L + t.v;
    }

    static long drive(Box b, Box t, int n, Shape s) {
        return sum(b, t, n, s) + 1;
    }

    static int sideCall(int i) {
        return (i * 7) & 15;
    }

    /** A local kept only for its read after the loop, with a call in the loop. */
    static long chain(Box keep, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += sideCall(i);
        }
        return acc + keep.v;
    }

    static long driveChain(Box keep, int n) {
        return chain(keep, n) * 3;
    }

    static String run(String tag, Box b, Box t, Shape s, boolean load) throws Exception {
        Thread loader = null;
        if (load) {
            loader = new Thread(() -> {
                while (phase != 1) {
                    Thread.onSpinWait();
                }
                try {
                    Thread.sleep(3);
                    Class.forName("R21FzKeepSplice1$Late");
                } catch (Exception e) {
                    throw new RuntimeException(e);
                }
            });
            loader.start();
        }
        phase = 1;
        String r;
        try {
            r = Long.toString(drive(b, t, N, s));
        } catch (NullPointerException e) {
            r = "NPE";
        }
        phase = 0;
        if (loader != null) {
            loader.join();
        }
        return "row " + tag + " " + r;
    }

    public static void main(String[] args) throws Exception {
        Box b = new Box(7);
        Box other = new Box(9);
        Shape one = new One();
        for (int rep = 0; rep < 30; rep++) {
            drive(b, b, 100_000, one);
            drive(b, other, 100_000, one);
            driveChain(b, 100_000);
        }
        System.out.println(run("same-warm", b, b, one, false));
        System.out.println(run("diff-warm", b, other, one, false));
        System.out.println(run("same-load", b, b, one, true));
        System.out.println("chain " + driveChain(b, N / 4));
        System.out.println(run("diff-after", b, other, one, false));
        Shape late = (Shape) Class.forName("R21FzKeepSplice1$Late").getDeclaredConstructor().newInstance();
        System.out.println(run("late-shape", b, b, late, false));
    }
}
