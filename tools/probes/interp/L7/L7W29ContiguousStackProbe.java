// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 29, lane L7: correctness of the slot slab (stage
// 1 of the contiguous interpreter stack, vm/src/runtime/slot_slab.rs). Every
// row exercises a way a frame's locals and operand stack can be disturbed
// when they live in one window of the frame stack's slab instead of the
// frame's own buffers: a callee's window laid next to a caller's operand
// stack that still holds pending values; category-2 argument slots (a long or
// double takes two local slots, one operand-stack slot); deep recursion that
// crosses slab chunks; an exception unwinding through thousands of windowed
// frames and the calls made after it (they reuse the released windows); a
// frame built by value (reflection) between windowed ones; a synchronized
// callee (the door's monitor path); a platform thread and a virtual thread,
// each with its own frame stack.
//
// Every frame re-checks its own locals after its callee returns, so a
// disturbed window prints a wrong number rather than passing by luck.
//
// Run: cratonvm -cp <dir> L7W29ContiguousStackProbe          (slab on)
//      CRATONVM_JIT_NO_LOCALS_SLAB=1 cratonvm -cp <dir> L7W29ContiguousStackProbe
//      both with and without --nojit; all four must print exactly HotSpot's
//      stdout. --compatible prints the same.
//
// HotSpot 25 (25.0.3) stdout, identical with -Xint:
//   sum-rec 500500
//   pending-long 2628741076
//   wide-args 3074457345618258602
//   mix-double 287684.5888671875
//   wide-rec 6666.5
//   unwind-top bottom 3000 1500.0
//   unwind-mid 1254514
//   unwind-mid-again 1254514
//   soe caught then 45150
//   refl-rec 7591
//   sync-rec 2001000
//   thread-deep 18003000 9000.0
//   virtual-deep 20100 yielded
//
// `soe` recurses to the VM's own limit (HotSpot: the 4 MB thread stack;
// CratonVM: `max_stack_depth`, 8192 frames) and prints only that it caught
// the error, so the depth does not reach stdout.
public class L7W29ContiguousStackProbe {

    // ---- recursion with a pending operand-stack value -----------------------

    static int sumRec(int n) {
        return n == 0 ? 0 : n + sumRec(n - 1);
    }

    // The caller's `l * 2` and `n` wait on its operand stack across the call.
    static long pendingLong(int n, long l) {
        if (n == 0) {
            return l;
        }
        return l * 2 + n + pendingLong(n - 1, l + 1) - l;
    }

    // ---- category-2 argument slots -------------------------------------------

    static long wide(long a, double b, int c, long d, double e, Object o, long f) {
        long local1 = a ^ d;
        double local2 = b + e;
        long local3 = (long) local2 + c + f + (o == null ? 0 : 1);
        return local1 + local3;
    }

    static double mixDouble(int n, long l, double d) {
        if (n == 0) {
            return d + l;
        }
        double here = d * 2;
        double r = mixDouble(n - 1, l * 3 + 1, d / 2) * 0.5 + n;
        return r + here - d * 2;
    }

    static double wideRec(int n, long a, double b) {
        long keepA = a;
        double keepB = b;
        if (n == 0) {
            return b;
        }
        double r = wideRec(n - 1, a + n, b + 0.5);
        if (keepA != a || keepB != b) {
            return -1;
        }
        return r + (a - keepA);
    }

    // ---- exceptions through many frames --------------------------------------

    static int thrower(int n, long l, double d) {
        if (n == 0) {
            throw new IllegalStateException("bottom " + l + " " + d);
        }
        return thrower(n - 1, l + 1, d + 0.5) + 1;
    }

    static String unwindTop(int depth) {
        try {
            thrower(depth, 0L, 0.0);
            return "no throw";
        } catch (IllegalStateException e) {
            return e.getMessage();
        }
    }

    static long catchMid(int n, int catchAt, long acc) {
        long mine = acc * 31 + n;
        if (n == 0) {
            throw new RuntimeException("x");
        }
        try {
            return catchMid(n - 1, catchAt, mine % 1_000_003);
        } catch (RuntimeException e) {
            if (n != catchAt) {
                throw e;
            }
            // This frame's locals survived the unwind below it; the call it
            // makes now reuses the windows the unwound frames released.
            return (mine % 1_000_003) + sumRec(n);
        }
    }

    // ---- stack overflow, then keep going ------------------------------------

    static int soeDepth;

    static void soe(int n, long l, double d) {
        soeDepth = n;
        soe(n + 1, l + 1, d + 1.0);
    }

    static String soeThenRecurse() {
        try {
            soe(0, 0L, 0.0);
            return "no soe";
        } catch (StackOverflowError e) {
            return "caught then " + sumRec(300);
        }
    }

    // ---- a by-value frame (reflection) between windowed ones ----------------

    static java.lang.reflect.Method REFL;

    public static long reflRec(int n, long acc) throws Exception {
        if (n == 0) {
            return acc;
        }
        long keep = acc;
        long r = (n % 3 == 0)
                ? (Long) REFL.invoke(null, n - 1, acc + 1)
                : reflRec(n - 1, acc + 1);
        return keep == acc ? r + 1 : -1;
    }

    // ---- synchronized callee --------------------------------------------------

    static synchronized long syncRec(int n) {
        return n == 0 ? 0 : n + syncRec(n - 1);
    }

    // ---- threads ------------------------------------------------------------

    static long threadRec(int n, long l, double d, double[] out) {
        if (n == 0) {
            out[0] = d;
            return l;
        }
        long r = threadRec(n - 1, l + n, d + 1.5, out);
        return r + (l & 1) - (l & 1);
    }

    static long vtRec(int n, long l) throws InterruptedException {
        if (n == 0) {
            Thread.sleep(1); // parks the virtual thread in the middle of the recursion
            return l;
        }
        long keep = l;
        long r = vtRec(n - 1, l + n);
        return keep == l ? r : -1;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("sum-rec " + sumRec(1000));
        System.out.println("pending-long " + pendingLong(2500, 1L << 20));
        System.out.println("wide-args " + wide(Long.MAX_VALUE, 1.5, 7, 0x5555_5555_5555_5555L,
                -1.5, null, -7L));
        System.out.println("mix-double " + mixDouble(30, 1L, 1e11));
        System.out.println("wide-rec " + wideRec(2000, 0L, 5666.5));
        System.out.println("unwind-top " + unwindTop(3000));
        System.out.println("unwind-mid " + catchMid(2500, 1200, 1L));
        System.out.println("unwind-mid-again " + catchMid(2500, 1200, 1L));

        String[] soeResult = new String[1];
        Thread soeThread = new Thread(null, () -> soeResult[0] = soeThenRecurse(), "soe", 4L << 20);
        soeThread.start();
        soeThread.join();
        System.out.println("soe " + soeResult[0]);

        REFL = L7W29ContiguousStackProbe.class.getDeclaredMethod("reflRec", int.class, long.class);
        System.out.println("refl-rec " + reflRec(90, 7411L));
        System.out.println("sync-rec " + syncRec(2000));

        long[] tl = new long[1];
        double[] td = new double[1];
        Thread deep = new Thread(null, () -> tl[0] = threadRec(6000, 0L, 0.0, td), "deep", 256L << 20);
        deep.start();
        deep.join();
        System.out.println("thread-deep " + tl[0] + " " + td[0]);

        long[] vl = new long[1];
        Thread vt = Thread.ofVirtual().start(() -> {
            try {
                vl[0] = vtRec(200, 0L);
            } catch (InterruptedException e) {
                vl[0] = -2;
            }
        });
        vt.join();
        System.out.println("virtual-deep " + vl[0] + " yielded");
    }
}
