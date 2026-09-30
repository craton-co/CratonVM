// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 38, lane L7: correctness of stage 2b of the
// contiguous interpreter stack -- the static and virtual doors lay the
// callee's locals over the caller's argument slots IN PLACE, with no copy of
// the arguments (`CRATONVM_JIT_OVERLAP_ARGS=1`,
// `invoke_fast::push_frame_in_place`, `FrameStack::push_cached_compact_in_place`).
// A `long` / `double` argument is one operand-stack slot and two local slots,
// so every argument after one moves up inside the window; these rows put
// category-2 arguments in every position, through both doors (the virtual
// door has a receiver in slot 0), and re-read every local after a callee
// returns so a slot two frames wrote prints a wrong number.
//
// Rows:
//   static-shift    cat-2 first, middle and last, mixed with ints and refs;
//   virtual-shift   the same through an instance method (receiver + cat-2);
//   virtual-rec     virtual recursion with long / double arguments;
//   iface           an interface call (the virtual door's itable arm);
//   gc              reference arguments after long ones across collections;
//   unwind          an exception thrown through in-place frames;
//   deep            recursion across slab chunks (declines, then overlaps
//                   again in the next chunk) with a double argument.
//
// Run: cratonvm -cp <dir> L7W38InPlaceArgsProbe                          (off)
//      CRATONVM_JIT_OVERLAP_ARGS=1 cratonvm -cp <dir> L7W38InPlaceArgsProbe   (on)
//      each with and without --nojit, and with --compatible; all must print
//      exactly HotSpot's stdout. Positive control for the "on" runs:
//      CRATONVM_DBG_INVOKE_PHASES=1 prints `[invoke-phases] arg overlap:
//      overlapped=N relaid_cat2=M declined=D released_at_return=R in_place=K`
//      on stderr with M > 0 and K > 0 under --nojit (K counts the in-place
//      installs among N + M), and all zero without the switch.
//
// HotSpot 25 (25.0.3) stdout, identical with -Xint:
//   static-shift 1.0000000023E10 -4.75 45
//   virtual-shift 40000000207 12.5 3
//   virtual-rec 5050 1300.0
//   iface 1000000000061 7.25
//   gc 499500 20000
//   unwind 17 caught 12
//   deep 4000.0 8002000
public class L7W38InPlaceArgsProbe {

    // ---- static door, category-2 in every position ---------------------------

    static double firstWide(long a, int b, Object c, int d) {
        int before = b + d;
        return a + before + (c == null ? 0 : 1000) + 0.0;
    }

    static double middleWide(int a, double b, int c, long d, int e) {
        return a * b + c - d + e;
    }

    static long lastWide(int a, Object b, int c, long d) {
        return a + c + d + (b == null ? 0 : 1);
    }

    // ---- virtual door, receiver + category-2 --------------------------------

    static class Calc {
        final long base;

        Calc(long base) {
            this.base = base;
        }

        long wideFirst(long a, int b, int c) {
            return base + a + b * c;
        }

        double wideMiddle(int a, double b, long c) {
            return a + b + c - base;
        }

        int narrow(int a, Object o, int b) {
            return a + b + (o == null ? 0 : 1);
        }

        long rec(long acc, int n, double unused) {
            return n == 0 ? acc : rec(acc + n, n - 1, unused + 0.5);
        }

        double recD(double acc, long n) {
            return n == 0 ? acc : recD(acc + n + 0.5, n - 1);
        }
    }

    interface Mixer {
        long mix(long a, int b, double c);

        double mixD(double a, long b);
    }

    static final class MixerImpl implements Mixer {
        public long mix(long a, int b, double c) {
            return a + b + (long) c;
        }

        public double mixD(double a, long b) {
            return a + b;
        }
    }

    // ---- references after long arguments across collections -----------------

    static final class Node {
        final int v;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static int churnAfterLong(long pad, Node head, double pad2, Node tail) {
        Object[] junk = new Object[32];
        for (int i = 0; i < 1500; i++) {
            junk[i & 31] = new long[128];
        }
        if (pad % 3 == 0) {
            System.gc();
        }
        int s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.v;
        }
        return s + tail.v * (int) pad2 * 0;
    }

    // ---- exceptions through in-place frames -----------------------------------

    static int throwAt(long depth, int limit, double x) {
        long mine = depth * 2;
        if (depth == limit) {
            throw new IllegalArgumentException("x");
        }
        return throwAt(depth + 1, limit, x + 1) + (int) mine;
    }

    // ---- recursion across slab chunks ----------------------------------------

    static double deepWide(double acc, long n, int step) {
        return n == 0 ? acc : deepWide(acc + step, n - 1, step);
    }

    static long deepSum(int n, long acc) {
        return n == 0 ? acc : deepSum(n - 1, acc + n);
    }

    public static void main(String[] args) {
        double a1 = 0;
        double a2 = 0;
        long a3 = 0;
        for (int i = 0; i < 3; i++) {
            a1 = firstWide(10_000_000_000L, 12, null, 11);
            a2 = middleWide(3, -1.25, 4, 7L, 2);
            a3 = lastWide(20, null, 25, 0L);
        }
        System.out.println("static-shift " + a1 + " " + a2 + " " + a3);

        Calc c = new Calc(7);
        long v1 = 0;
        double v2 = 0;
        int v3 = 0;
        for (int i = 0; i < 3; i++) {
            v1 = c.wideFirst(40_000_000_000L, 20, 10);
            v2 = c.wideMiddle(5, 1.5, 13L);
            v3 = c.narrow(1, new Object(), 1);
        }
        System.out.println("virtual-shift " + v1 + " " + v2 + " " + v3);

        System.out.println("virtual-rec " + c.rec(0L, 100, 0.0) + " " + c.recD(0.0, 50));

        Mixer m = new MixerImpl();
        long mx = 0;
        double md = 0;
        for (int i = 0; i < 3; i++) {
            mx = m.mix(1_000_000_000_000L, 60, 1.9);
            md = m.mixD(0.25, 7L);
        }
        System.out.println("iface " + mx + " " + md);

        Node head = null;
        for (int i = 0; i < 1000; i++) {
            head = new Node(i, head);
        }
        Node tail = new Node(20000, null);
        int g = 0;
        for (long p = 0; p < 6; p++) {
            g = churnAfterLong(p, head, 1.0, tail);
        }
        System.out.println("gc " + g + " " + tail.v);

        int u = 0;
        String caught = "none";
        try {
            u = throwAt(0L, 12, 0.5);
        } catch (IllegalArgumentException e) {
            caught = "caught";
        }
        System.out.println("unwind " + (u + 17) + " " + caught + " " + throwAtSafe());

        System.out.println("deep " + deepWide(0.0, 4000, 1) + " " + deepSum(4000, 0L));
    }

    static int throwAtSafe() {
        try {
            return throwAt(0L, 12, 0.0);
        } catch (IllegalArgumentException e) {
            return 12;
        }
    }
}
