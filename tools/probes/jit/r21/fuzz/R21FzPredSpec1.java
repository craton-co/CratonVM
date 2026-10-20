// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-2, per-planter speculation ids for
// the loop-entry predicates (`CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`,
// `deopt::predicate_speculation_id` / `predicate_withdrawn`,
// `ir_optimize::tag_predicate_guards`, the trapped-callee charge in
// `vm/src/jit/helpers.rs` `predicate_trap_charge`).
//
// Risk targeted: under the switch a failing guard withdraws ITS planter only,
// so the next compile still plants the OTHER predicates at the same header,
// and the trap count at `(UncommonTrap, header bci)` stays shared. A guard
// stamped with the wrong planter id (or a planter that keeps planting after
// its own trap limit) would loop trap -> recompile, or -- worse -- a
// predicate that is withdrawn in one planter's census but still assumed by
// another's check elimination would drop an in-loop check. Every row here
// alternates inputs that break exactly one predicate of a header that holds
// several (range on `a[i + off]`, mask on `a[(i + off) & m]`, class on an
// invariant receiver, length on a buffer receiver), more than four times
// each, and prints the exact exception and the partial progress (a static
// written every trip) at the faulting iteration.
//
// Run:
//   javac -d out R21FzPredSpec1.java
//   java -Xmx256m -cp out R21FzPredSpec1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzPredSpec1
//   CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1 cratonvm -Xmx256m -cp out R21FzPredSpec1
//   CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1 CRATONVM_DBG_LICM=1 ... (engagement: `de-specialised, not planted` lines name one planter)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.nio.ByteBuffer;

public class R21FzPredSpec1 {
    static final int WARM = 40_000;
    static int sTrip;

    interface Shape {
        int k();
    }

    static final class One implements Shape {
        public int k() {
            return 1;
        }
    }

    static final class Two implements Shape {
        public int k() {
            return 2;
        }
    }

    static final class Three implements Shape {
        public int k() {
            return 3;
        }
    }

    /** range predicate on a[i + off] and class predicate on s, one header. */
    static long multi(int[] a, int off, int n, Shape s) {
        long t = 0;
        for (int i = 0; i < n; i++) {
            sTrip = i;
            t += (long) a[i + off] * s.k();
        }
        return t;
    }

    /** mask predicate on (i + off) & m. */
    static long masked(int[] a, int off, int m, int n) {
        long t = 0;
        for (int i = 0; i < n; i++) {
            sTrip = i;
            t += a[(i + off) & m];
        }
        return t;
    }

    /** decreasing index with a stride (monotone predicate). */
    static long down(int[] a, int n, int stride) {
        long t = 0;
        for (int i = n - 1; i >= 0; i--) {
            sTrip = i;
            t += a[i * stride];
        }
        return t;
    }

    /** length predicate on an invariant buffer receiver, heap or direct. */
    static long buf(ByteBuffer b, int off, int n) {
        long t = 0;
        for (int i = 0; i < n; i++) {
            sTrip = i;
            t += b.get(i + off);
        }
        return t;
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    interface Row {
        long run();
    }

    static void row(String tag, String name, Row r) {
        sTrip = -1;
        String out;
        try {
            out = Long.toString(r.run());
        } catch (RuntimeException e) {
            out = exc(e);
        }
        System.out.println("row " + tag + " " + name + " -> " + out + " trip=" + sTrip);
    }

    static final int[] A = new int[64];
    static final ByteBuffer HEAP = ByteBuffer.allocate(64);
    static final ByteBuffer DIRECT = ByteBuffer.allocateDirect(64);

    static {
        for (int i = 0; i < 64; i++) {
            A[i] = i * 3 + 1;
            HEAP.put(i, (byte) (i * 7));
            DIRECT.put(i, (byte) (i * 5 + 1));
        }
    }

    static void rows(String tag, Shape s) {
        row(tag, "multi-ok", () -> multi(A, 4, 50, s));
        row(tag, "multi-high", () -> multi(A, 20, 50, s));
        row(tag, "multi-neg", () -> multi(A, -3, 50, s));
        row(tag, "multi-zero-trip-bad", () -> multi(A, 1000, 0, s));
        row(tag, "masked-ok", () -> masked(A, 9, 63, 100));
        row(tag, "masked-wide", () -> masked(A, 9, 127, 100));
        row(tag, "down-ok", () -> down(A, 21, 3));
        row(tag, "down-bad", () -> down(A, 33, 2));
        row(tag, "buf-heap", () -> buf(HEAP, 3, 60));
        row(tag, "buf-direct", () -> buf(DIRECT, 3, 60));
        row(tag, "buf-heap-bad", () -> buf(HEAP, 10, 60));
        row(tag, "buf-direct-bad", () -> buf(DIRECT, 10, 60));
    }

    public static void main(String[] args) {
        Shape one = new One();
        rows("cold", one);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + multi(A, r & 7, 40, one);
            h = h * 31 + masked(A, r, 63, 40);
            h = h * 31 + down(A, 20, 3);
            h = h * 31 + buf(HEAP, r & 3, 40);
        }
        System.out.println("warm hash " + h);
        rows("hot", one);
        // Break one predicate at a time, more than the per-bci trap limit.
        Shape two = new Two();
        long g = 0;
        int caught = 0;
        for (int round = 0; round < 6; round++) {
            for (int r = 0; r < 5000; r++) {
                g = g * 31 + multi(A, r & 7, 40, (r & 1) == 0 ? one : two);
                g = g * 31 + buf((r & 1) == 0 ? HEAP : DIRECT, r & 3, 40);
            }
            try {
                multi(A, 30, 40, one);
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++;
            }
            try {
                masked(A, 1, 255, 70);
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++;
            }
            try {
                buf(DIRECT, 40, 40);
            } catch (IndexOutOfBoundsException e) {
                caught++;
            }
            rows("round" + round, (round & 1) == 0 ? two : one);
        }
        System.out.println("churn hash " + g + " caught=" + caught);
        Shape three = new Three();
        rows("late", three);
        rows("after", one);
    }
}
