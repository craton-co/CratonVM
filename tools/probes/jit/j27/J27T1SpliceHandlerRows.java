// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 6 (lane T1SPLICE): the rows item (3) of the R21CfChain section of
// docs/feature-designs/jit-ir-optimizations.md still lacked
// once CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS and ..._WIDE went default ON
// (JIT r24): a spliced callee's OWN `catch (Throwable)` must see every
// exception its covered range raises, as on HotSpot, where C2 inlines the
// callee together with its handler. The r22 flip review (S1) named the
// missing one: an allocation in the covered range that fails with
// OutOfMemoryError. The other two are the residual risk the book recorded
// (a StackOverflowError leaving through the caller's route) and the
// uniApplyNow shape itself (handler runs code and reads a pre-`try` local).
//
// Rows (each kernel is a static callee called from a hot loop, so it is a
// splice candidate with no receiver profile):
//   wrap   stage(d, o): try { new Cell(((String) o).length()) } catch (Throwable)
//          { d.cce++ | d.npe++ | d.other++ }. Per 1000 iterations: 998 Strings
//          of length 4, one null (NPE at length()), one Integer (CCE).
//   soe    guarded(k): try { recurse(k) } catch (Throwable t) { -7 for a
//          StackOverflowError }. The last iteration of each rep asks 2^30 frames.
//   oom    push(d): try { d.head = new Node(d.head) } catch (Throwable t)
//          { d.head = null; d.caught = t; return 1 }. Warmed with resets, then
//          filled until the heap is exhausted. Needs -Xmx64m (skipped above
//          256 MiB of max heap, the row would take minutes).
//
// Run:
//   javac -d out tools/probes/jit/j27/J27T1SpliceHandlerRows.java
//   java -Xmx64m -cp out J27T1SpliceHandlerRows > hs.txt                       # HotSpot 25
//   cratonvm -Xmx64m -cp out J27T1SpliceHandlerRows | diff - hs.txt            # --real-jdk
//   cratonvm --compatible -Xmx64m -cp out J27T1SpliceHandlerRows | diff - hs.txt
// Arms (each must print the HotSpot lines):
//   default (handler family ON);  CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE=0;
//   CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS=0;  --nojit (reference, slow)
// Engagement (CRATONVM_DBG_JITC=1 2>&1): for stage / guarded / push either a
//   splice of the body into run* or `callee-exception-table` / `ir-splice-handler
//   REFUSED` (a refused splice keeps the call: correct, not engaged). A wrong
//   line with the splice planned is the defect this probe exists for.
//
// Expected HotSpot 25 output (worked out from JVMS semantics; `bad` counts reps
// whose answer differed from rep 0):
//   fact wrap sum=797800 cce=200 npe=200 other=0 bad=0
//   fact soe acc=799985 bad=0
//   fact oom rounds=2 caught=java.lang.OutOfMemoryError,java.lang.OutOfMemoryError
// (`fact oom skipped` when the max heap is above 256 MiB.)
// Timing (only with -Dj27.time=true): `time wrap <ns>/op`.

public class J27T1SpliceHandlerRows {
    static final int REPS = 5;

    static final class Cell {
        final int v;

        Cell(int v) {
            this.v = v;
        }
    }

    static final class Holder {
        int res;
        int cce;
        int npe;
        int other;
    }

    static final String STR = "abcd";
    static final Object INT = Integer.valueOf(7);

    static int stage(Holder d, Object o) {
        try {
            Cell c = new Cell(((String) o).length());
            d.res = c.v;
        } catch (Throwable ex) {
            if (ex instanceof ClassCastException) {
                d.cce++;
                d.res = -1;
            } else if (ex instanceof NullPointerException) {
                d.npe++;
                d.res = -2;
            } else {
                d.other++;
                d.res = -3;
            }
        }
        return d.res;
    }

    static String runWrap(int n) {
        Holder d = new Holder();
        long sum = 0;
        for (int i = 0; i < n; i++) {
            int m = i % 1000;
            Object o = m == 999 ? INT : (m == 500 ? null : STR);
            sum += stage(d, o);
        }
        return "sum=" + sum + " cce=" + d.cce + " npe=" + d.npe + " other=" + d.other;
    }

    static int recurse(int k) {
        return k == 0 ? 0 : 1 + recurse(k - 1);
    }

    static int guarded(int k) {
        try {
            return recurse(k);
        } catch (Throwable t) {
            return t instanceof StackOverflowError ? -7 : -8;
        }
    }

    static long runSoe(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += guarded(i == n - 1 ? 1 << 30 : 8);
        }
        return acc;
    }

    static final class Node {
        final Node next;

        Node(Node next) {
            this.next = next;
        }
    }

    static final class Chain {
        Node head;
        Throwable caught;
    }

    static int push(Chain d) {
        try {
            d.head = new Node(d.head);
            return 0;
        } catch (Throwable t) {
            d.head = null;
            d.caught = t;
            return 1;
        }
    }

    static int fillChain(Chain d, int n) {
        int r = 0;
        for (int i = 0; i < n && r == 0; i++) {
            r = push(d);
        }
        return r;
    }

    static String runOom() {
        Chain d = new Chain();
        for (int w = 0; w < 30; w++) {
            fillChain(d, 20_000);
            d.head = null;
        }
        StringBuilder names = new StringBuilder();
        for (int round = 0; round < 2; round++) {
            d.caught = null;
            int r = 0;
            while (r == 0) {
                r = fillChain(d, Integer.MAX_VALUE);
            }
            d.head = null;
            if (round > 0) {
                names.append(',');
            }
            names.append(d.caught == null ? "none" : d.caught.getClass().getName());
        }
        d.caught = null;
        return "rounds=2 caught=" + names;
    }

    public static void main(String[] args) {
        String wrap0 = null;
        int wrapBad = 0;
        for (int r = 0; r < REPS; r++) {
            String w = runWrap(200_000);
            if (wrap0 == null) {
                wrap0 = w;
            } else if (!wrap0.equals(w)) {
                wrapBad++;
            }
        }
        System.out.println("fact wrap " + wrap0 + " bad=" + wrapBad);

        long soe0 = 0;
        int soeBad = 0;
        for (int r = 0; r < REPS; r++) {
            long a = runSoe(100_000);
            if (r == 0) {
                soe0 = a;
            } else if (a != soe0) {
                soeBad++;
            }
        }
        System.out.println("fact soe acc=" + soe0 + " bad=" + soeBad);

        if (Runtime.getRuntime().maxMemory() > (256L << 20)) {
            System.out.println("fact oom skipped");
        } else {
            System.out.println("fact oom " + runOom());
        }

        if (Boolean.getBoolean("j27.time")) {
            int n = 2_000_000;
            runWrap(n);
            long t0 = System.nanoTime();
            String w = runWrap(n);
            long t1 = System.nanoTime();
            System.out.println("time wrap " + ((t1 - t0) / n) + "/op (" + w.length() + ")");
        }
    }
}
