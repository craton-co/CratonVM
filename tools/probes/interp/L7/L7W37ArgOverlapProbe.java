// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 37, lane L7: correctness of stage 2 of the
// contiguous interpreter stack, argument overlap (`CRATONVM_JIT_OVERLAP_ARGS=1`,
// `FrameStack::push_cached_compact_overlapping`). With it on, a callee the
// fast invoke doors push takes its locals ON the argument slots its caller
// pushed, so every row below exercises a way that sharing can go wrong:
//
//   * the callee writes its argument locals and pushes deep on its own
//     operand stack while the caller's pending values sit just below;
//   * `long` / `double` arguments (one operand-stack slot, two local slots:
//     the arguments are laid again), mixed with int and reference ones;
//   * a value return lands on the callee's first slot (the arms release the
//     callee's view first), for every return kind, and argument-producing
//     calls `f(g(1), g(2))` whose results become the next call's arguments;
//   * reference arguments across collections the callee triggers;
//   * exceptions thrown through overlapped frames and caught at every level;
//   * recursion deep enough to cross slab chunks (the overlap declines at a
//     chunk boundary and resumes in the next chunk);
//   * the virtual and special doors, a synchronized callee, a call with more
//     parameters than a door takes (general path) between overlapped ones;
//   * a second platform thread.
//
// Every frame re-reads its own locals after its callee returns, so a slot two
// frames wrote prints a wrong number rather than passing by luck.
//
// Run: cratonvm -cp <dir> L7W37ArgOverlapProbe                         (off)
//      CRATONVM_JIT_OVERLAP_ARGS=1 cratonvm -cp <dir> L7W37ArgOverlapProbe  (on)
//      each with and without --nojit, and with --compatible; all must print
//      exactly HotSpot's stdout. Positive control for the "on" runs:
//      CRATONVM_DBG_INVOKE_PHASES=1 prints
//      `[invoke-phases] arg overlap: overlapped=N relaid_cat2=M declined=D
//      released_at_return=R` on stderr with N, M, R all non-zero under
//      --nojit (and all zero without the switch).
//
// HotSpot 25 (25.0.3) stdout, identical with -Xint:
//   pending 5050 -7 99
//   mutate-args 1275 12 1
//   cat2-args 1.23456789009965E13 -2.5 77
//   cat2-rec 2500000.0 1251250000
//   return-kinds 42 -9000000000 2.5 1.25 true x ok
//   arg-calls 30 3000 0.5
//   refs-gc 499501 1000 1000
//   unwind 3 99 1275 mid-caught 1234
//   deep 45150 3000.0 4501500
//   virtual 1275 -1275 60
//   sync 2001000
//   wide 55 1275 55
//   thread 18003000 9000.0
public class L7W37ArgOverlapProbe {

    // ---- pending caller values below the arguments ---------------------------

    // `n` and the constant wait on the caller's operand stack under the args.
    static int pendingRec(int n, int bias) {
        if (n == 0) {
            return bias;
        }
        return n + pendingRec(n - 1, bias) - bias + bias;
    }

    static int stackHungry(int a, int b, int c) {
        // Pushes deeper than the argument count, then rewrites every argument.
        int r = ((a * 3 + b) * (c + 2) - (a - b) * (b - c)) + ((a + b + c) * (a - c));
        a = r;
        b = r * 2;
        c = -r;
        return a + b + c - r;
    }

    // ---- the callee rewrites its argument locals, then calls deeper ----------

    static int mutateArgs(int n, int acc, int depthMark) {
        int saved = n;
        n = n * 2;
        acc = acc + saved;
        if (saved == 0) {
            return acc;
        }
        int deeper = mutateArgs(saved - 1, acc, depthMark + 1) - acc;
        // n and acc are this frame's own: the callee's slots were its args.
        return acc + deeper + (n / 2 - saved);
    }

    // ---- category-2 arguments -------------------------------------------------

    static double cat2(long a, int b, double c, Object o, long d) {
        long x = a + d;
        double y = c * b;
        b = 0;
        a = 0;
        return x + y + (o == null ? 0 : 1) + b + a;
    }

    static double cat2Rec(double acc, long n, int k) {
        if (n == 0) {
            return acc;
        }
        double here = acc + k;
        return cat2Rec(here, n - 1, k) + 0 * here;
    }

    static long cat2Long(long n, int k, long acc) {
        if (n == 0) {
            return acc;
        }
        return cat2Long(n - 1, k, acc + n * k);
    }

    // ---- every return kind, landing on the callee's first slot ---------------

    static int rInt(int a, int b) { return a * b; }
    static long rLong(long a, int b) { return a * b; }
    static double rDouble(double a, double b) { return a / b; }
    static float rFloat(float a) { return a / 2; }
    static boolean rBool(int a) { return a > 0; }
    static String rRef(String s, int i) { return s.substring(i, i + 1); }
    static void rVoid(int[] box, int v) { box[0] = v; }

    // ---- results of calls as the next call's arguments -----------------------

    static int g(int x) {
        int t = x * 10;
        return t;
    }

    static int f2(int a, int b) { return a + b; }

    static long gl(long x) { return x * 1000; }

    static long fl(long a, long b, int c) { return a + b + c; }

    static double gd(int a, double b) { return a * b; }

    // ---- references across collections ---------------------------------------

    static final class Box {
        final int v;
        final Box next;
        Box(int v, Box next) {
            this.v = v;
            this.next = next;
        }
    }

    static int churn(Box head, int n, Box tail) {
        // Allocate enough to collect while `head` and `tail` are only this
        // frame's (overlapped) locals.
        Object[] junk = new Object[64];
        for (int i = 0; i < 2000; i++) {
            junk[i & 63] = new int[256];
        }
        if (n % 250 == 0) {
            System.gc();
        }
        int s = 0;
        for (Box b = head; b != null; b = b.next) {
            s += b.v;
        }
        return s + tail.v * 0;
    }

    static int refsGc() {
        Box head = null;
        for (int i = 0; i < 1000; i++) {
            head = new Box(i, head);
        }
        Box tail = new Box(1000, null);
        int last = 0;
        for (int n = 0; n < 1000; n += 125) {
            last = churn(head, n, tail);
        }
        int len = 0;
        for (Box b = head; b != null; b = b.next) {
            len++;
        }
        return last * 1000 + len;
    }

    // ---- exceptions through overlapped frames ---------------------------------

    static int thrower(int n, int limit) {
        int mine = n * 7;
        if (n == limit) {
            throw new IllegalStateException("at " + n);
        }
        return thrower(n + 1, limit) + mine;
    }

    static int catchAtMid(int n, int acc) {
        if (n == 5) {
            try {
                return thrower(0, 20);
            } catch (IllegalStateException e) {
                return acc + 1234 - acc;
            }
        }
        return catchAtMid(n + 1, acc + n);
    }

    // ---- recursion across slab chunks -----------------------------------------

    static int deepInt(int n) {
        return n == 0 ? 0 : n + deepInt(n - 1);
    }

    static double deepDouble(double acc, int n) {
        return n == 0 ? acc : deepDouble(acc + 1.0, n - 1);
    }

    static long deepMixed(long a, int n, Object o) {
        if (n == 0) {
            return a;
        }
        long r = deepMixed(a + n, n - 1, o);
        return r + (o == null ? 0 : 1);
    }

    // ---- virtual / special doors, synchronized, wide --------------------------

    static class Acc {
        int base;
        Acc(int base) { this.base = base; }
        int add(int a, int b) { return base + a + b; }
        final int neg(int a) { return -a; }
        private int priv(int a, int b, int c) { return a * b * c; }
        int viaPrivate(int x) { return priv(x, 3, 4) + 0; }
        synchronized int syncRec(int n) { return n == 0 ? 0 : n + syncRec(n - 1); }
    }

    static int virtualRec(Acc acc, int n) {
        if (n == 0) {
            return 0;
        }
        return acc.add(n, 0) - acc.base + virtualRec(acc, n - 1);
    }

    static int wide10(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j) {
        return a + b + c + d + e + f + g + h + i + j;
    }

    static int wideThenNarrow(int n) {
        if (n == 0) {
            return 0;
        }
        int w = wide10(n, 0, 0, 0, 0, 0, 0, 0, 0, 0);
        return w + wideThenNarrow(n - 1);
    }

    // ---- a second thread -------------------------------------------------------

    static long threadWork(int n, long acc) {
        return n == 0 ? acc : threadWork(n - 1, acc + n);
    }

    public static void main(String[] args) throws Exception {
        // Warm enough that every door's inline cache is filled.
        int p = 0;
        for (int i = 0; i < 3; i++) {
            p = pendingRec(100, -7);
        }
        System.out.println("pending " + (p + 7 + 5050 - 5050 + 0) + " " + (-7) + " " + (99 + stackHungry(1, 2, 3) * 0));

        int m = 0;
        for (int i = 0; i < 3; i++) {
            m = mutateArgs(50, 0, 0);
        }
        System.out.println("mutate-args " + m + " " + stackHungry(1, 2, 3) + " " + (stackHungry(2, 2, 2) / 25));

        double c2 = 0;
        for (int i = 0; i < 3; i++) {
            c2 = cat2(12_345_678_900_000L, -2, 1.75, null, 1_000L);
        }
        double c2b = cat2(0L, 7, -0.5, new Object(), 0L);
        System.out.println("cat2-args " + c2 + " " + c2b + " " + (int) cat2(70L, 1, 7.0, null, 0L));

        System.out.println("cat2-rec " + cat2Rec(0.0, 1000, 2500) + " " + cat2Long(1000, 2500, 0L));

        int[] box = new int[1];
        rVoid(box, 5);
        System.out.println("return-kinds " + rInt(6, 7) + " " + rLong(-3_000_000_000L, 3) + " "
                + rDouble(5.0, 2.0) + " " + rFloat(2.5f) + " " + rBool(box[0]) + " "
                + rRef("xyz", 0) + " " + (box[0] == 5 ? "ok" : "bad"));

        System.out.println("arg-calls " + f2(g(1), g(2)) + " " + fl(gl(1), gl(2), (int) gl(0)) + " "
                + gd(f2(g(0), 1), 0.5));

        System.out.println("refs-gc " + (refsGc() / 1000) + " " + (refsGc() % 1000 == 0 ? 1000 : refsGc() % 1000) + " 1000");

        int u = 0;
        try {
            thrower(0, 3);
        } catch (IllegalStateException e) {
            u = e.getMessage().length() - 1;
        }
        int after = 99;
        int again = mutateArgs(50, 0, 0);
        System.out.println("unwind " + u + " " + after + " " + again + " mid-caught " + catchAtMid(0, 0));

        System.out.println("deep " + deepInt(300) + " " + deepDouble(0.0, 3000) + " " + deepMixed(0L, 3000, null));

        Acc acc = new Acc(9);
        System.out.println("virtual " + virtualRec(acc, 50) + " " + (-virtualRec(acc, 50)) + " " + acc.viaPrivate(5));

        System.out.println("sync " + new Acc(0).syncRec(2000));

        System.out.println("wide " + wide10(1, 2, 3, 4, 5, 6, 7, 8, 9, 10) + " " + wideThenNarrow(50) + " "
                + f2(wide10(1, 2, 3, 4, 5, 6, 7, 8, 9, 10), 0));

        final long[] tr = new long[1];
        final double[] td = new double[1];
        Thread t = new Thread(null, () -> {
            tr[0] = threadWork(6000, 0L);
            td[0] = deepDouble(0.0, 9000);
        }, "deep", 256L << 20);
        t.start();
        t.join();
        System.out.println("thread " + tr[0] + " " + td[0]);
    }
}
