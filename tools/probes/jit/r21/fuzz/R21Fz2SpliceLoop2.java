// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: a DEOPT inside a spliced callee's promoted
// loop (`CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED`; the static, fresh-object and
// array-element promotions reach spliced loops through the same
// `promotion_header_frame`).
//
// Risk targeted: while the loop runs, the field's current value lives only
// in a register. A deopt inside the loop (a type guard on the element the
// loop calls through, a class first seen mid-loop) must rebuild the CALLEE
// frame and the caller frame with the field holding every trip's update so
// far; the interpreter continues from there. The lowering refuses a poll
// exit at a promoted spliced header; every OTHER exit (a guard's deopt) is
// what this probe drives. Shapes: an instance accumulator with a virtual
// call on each element (the guard), a static accumulator (`Tot.total`) in a
// spliced static callee, an array element `acc[k] += ...` in a spliced callee
// (`CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`), and a nested loop (outer in
// the caller, inner in the callee). Each runs hot on elements of one class,
// then on an array where the new class appears at a known trip; the results
// are exact sums.
//
// Run:
//   javac -d out R21Fz2SpliceLoop2.java
//   java -Xmx256m -cp out R21Fz2SpliceLoop2                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2SpliceLoop2
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 cratonvm -Xmx256m -cp out R21Fz2SpliceLoop2
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm ...
//   ... + CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1; CRATONVM_DBG_DEOPT=1 (stderr: where it deopted)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2SpliceLoop2 {
    static final int WARM = 60_000;

    interface Val {
        int get();
    }

    static final class One implements Val {
        final int v;

        One(int v) {
            this.v = v;
        }

        public int get() {
            return v;
        }
    }

    static final class Neg implements Val {
        final int v;

        Neg(int v) {
            this.v = v;
        }

        public int get() {
            return -v * 10;
        }
    }

    static final class Acc {
        int sum;
        long wide;

        void eat(Val[] vs) {
            for (int i = 0; i < vs.length; i++) {
                int g = vs[i].get();
                sum += g;
                wide += (long) g << 20;
            }
        }
    }

    static final class Tot {
        static int total;

        static void eat(Val[] vs) {
            for (int i = 0; i < vs.length; i++) {
                total += vs[i].get();
            }
        }
    }

    static void eatInto(int[] acc, int k, Val[] vs) {
        for (int i = 0; i < vs.length; i++) {
            acc[k] += vs[i].get();
        }
    }

    static long callerA(Acc a, Val[] vs) {
        a.eat(vs);
        return a.sum * 1_000_000L + (a.wide >> 20);
    }

    static long callerS(Val[] vs) {
        Tot.total = 7;
        Tot.eat(vs);
        return Tot.total;
    }

    static long callerE(int[] acc, Val[] vs) {
        acc[1] = 3;
        eatInto(acc, 1, vs);
        return acc[1] * 10L + acc[0] + acc[2];
    }

    static long callerN(Acc a, Val[][] vss) {
        long t = 0;
        for (Val[] vs : vss) {
            a.eat(vs);
            t = t * 31 + a.sum;
        }
        return t;
    }

    static Val[] ones(int n, int lateAt) {
        Val[] vs = new Val[n];
        for (int i = 0; i < n; i++) {
            vs[i] = i == lateAt ? new Neg(i) : new One(i + 1);
        }
        return vs;
    }

    static String row(Val[] vs) {
        int[] acc = {100, 0, 200};
        return callerA(new Acc(), vs) + " " + callerS(vs) + " " + callerE(acc, vs) + " "
                + callerN(new Acc(), new Val[][] {vs, vs, ones(5, -1)});
    }

    public static void main(String[] args) {
        Val[] warm = ones(40, -1);
        System.out.println("row cold | " + row(warm));
        long h = 0;
        int[] acc = new int[3];
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + callerA(new Acc(), warm) + callerS(warm) + callerE(acc, warm);
            if ((r & 7) == 0) {
                h = h * 31 + callerN(new Acc(), new Val[][] {warm, warm});
            }
        }
        System.out.println("warm hash " + h);
        System.out.println("row hot | " + row(warm));
        // The deopt trigger: Neg first seen at trip 0, 17 and 39 of the
        // promoted loop (first, middle, last), and in a long loop at 3000.
        System.out.println("row late0 | " + row(ones(40, 0)));
        System.out.println("row late17 | " + row(ones(40, 17)));
        System.out.println("row late39 | " + row(ones(40, 39)));
        System.out.println("row long | " + row(ones(5000, 3000)));
        System.out.println("row after | " + row(warm));
        for (int r = 0; r < WARM / 2; r++) {
            h = h * 31 + callerA(new Acc(), (r & 15) == 0 ? ones(40, r % 40) : warm) + callerS(warm);
        }
        System.out.println("rewarm hash " + h);
        System.out.println("row final | " + row(ones(40, 21)));
    }
}
