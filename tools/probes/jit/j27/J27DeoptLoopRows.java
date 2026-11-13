// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 2, lane DEOPTREV: facts for the deopt / loop-predication /
 * unroll / promoted-cell paths the wave-2 review read
 * (docs/known-issues/jit/j27-deoptrev-review-rows-20261028.md).
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in both modes, with
 * and without CRATONVM_JIT_IR_UNROLL_CFG_BODY=1. Each row warms its method
 * with in-range arguments (so the optimizing tier compiles it with the loop
 * predicates, promotions and unrolls on), then calls it once with arguments
 * that take the exceptional / deopt path. The expected lines are listed in the
 * page.
 *
 * No timing lines.
 */
public final class J27DeoptLoopRows {
    static final int WARM = 30_000;

    static int COUNT;
    static long SUM;
    static int EVEN;
    static int ODD;
    static int LAST_I;
    static Object KEEP;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static void thrown(String name, Throwable t) {
        fact(name, t.getClass().getName() + ": " + t.getMessage());
    }

    // ── (d) promoted cells at an exceptional exit ─────────────────────────

    /** Two static cells (int, long) across an array read that throws. */
    static void staticCells(int[] a, int n) {
        for (int i = 0; i < n; i++) {
            COUNT++;
            SUM += a[i];
        }
    }

    /** A static cell across an integer division that throws. */
    static int divCells(int[] d, int n) {
        int q = 0;
        for (int i = 0; i < n; i++) {
            COUNT += 2;
            q += 1000 / d[i];
        }
        return q;
    }

    static final class Acc {
        int total;
        long hits;
    }

    /** Field cells of an invariant receiver across an array read that throws. */
    static void fieldCells(Acc acc, int[] a, int n) {
        for (int i = 0; i < n; i++) {
            acc.hits++;
            acc.total += a[i];
        }
    }

    /** An array element cell stored before the throwing read of the trip. */
    static int elemCell(long[] cell, int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            cell[0] += 7;
            s += a[i];
        }
        return s;
    }

    /** A fresh object's field cell, read back by the handler. */
    static int freshCell(int[] a, int n) {
        Acc acc = new Acc();
        try {
            for (int i = 0; i < n; i++) {
                acc.hits += 3;
                acc.total += a[i];
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            return (int) acc.hits * 1000 + acc.total;
        }
        return -1;
    }

    // ── (b) loop predicates ───────────────────────────────────────────────

    /** The decode loop's monotone shape (UTF_8$Decoder.decodeArrayLoop). */
    static int monotone(byte[] sa, int sp, int sl, char[] da, int dp, int dl) {
        while (sp < sl) {
            int b = sa[sp];
            if (b >= 0) {
                if (dp >= dl) {
                    return -dp;
                }
                da[dp] = (char) b;
                sp++;
                dp++;
            } else {
                sp++;
            }
        }
        return dp;
    }

    /**
     * A pre-loop division whose quotient the loop-entry frame names, ahead of
     * the monotone predicates: a failing predicate resumes at the header,
     * which must see `q` computed (and a zero divisor must throw first).
     */
    static int divThenMonotone(byte[] sa, int sl, char[] da, int dl, int x, int y) {
        int q = x / y;
        int sp = 0;
        int dp = 0;
        while (sp < sl) {
            int b = sa[sp];
            if (b >= 0) {
                if (dp >= dl) {
                    return q * 100 - dp;
                }
                da[dp] = (char) b;
                sp++;
                dp++;
            } else {
                sp++;
            }
        }
        return q * 100 + dp;
    }

    /** A down count from an invariant start. */
    static int down(int[] a, int n) {
        int s = 0;
        for (int i = n; i >= 0; i--) {
            s += a[i];
        }
        return s;
    }

    /** A `!=` exit on the array's own length from a variable start. */
    static int neExit(int[] a, int start) {
        int t = 0;
        for (int i = start; i != a.length; i++) {
            t += a[i];
        }
        return t;
    }

    /** A range-predicated loop whose bound passes the array. */
    static int upTo(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            COUNT++;
            s += a[i];
        }
        return s;
    }

    // ── (c) partial unroll remainders, loop retire, cfg unroll ────────────

    static long tri(int start, int n) {
        long s = 0;
        for (int i = start; i < n; i++) {
            s += i;
        }
        return s;
    }

    static int stride2(int start, int n) {
        int c = 0;
        for (int i = start; i < n; i += 2) {
            c ^= i;
            c++;
        }
        return c;
    }

    static long ltri(long start, long n) {
        long s = 0;
        for (long i = start; i < n; i++) {
            s += i & 7;
        }
        return s;
    }

    static long neWrap(int s, int n) {
        long c = 0;
        int i;
        for (i = s; i != n; i++) {
            c += 5;
        }
        LAST_I = i;
        return c;
    }

    static int inclUp(int s, int n) {
        int c = 0;
        for (int i = s; i <= n; i++) {
            c += 3;
        }
        return c;
    }

    static int inclDown(int s, int n) {
        int c = 0;
        for (int i = s; i >= n; i--) {
            c += 2;
        }
        return c;
    }

    static int diamond(int a, int b) {
        int x = 0;
        for (int k = 0; k < 8; k++) {
            x += (k & 1) == 0 ? a : b;
        }
        return x;
    }

    static void evenOdd() {
        for (int k = 0; k < 5; k++) {
            if ((k & 1) == 0) {
                EVEN++;
            } else {
                ODD++;
            }
        }
    }

    // ── (a) deopt: locks, class guards, materialisation ───────────────────

    interface Shape {
        int area();
    }

    static final class Sq implements Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        public int area() {
            return s * s;
        }
    }

    static final class Rect implements Shape {
        final int w;
        final int h;

        Rect(int w, int h) {
            this.w = w;
            this.h = h;
        }

        public int area() {
            return w * h;
        }
    }

    static final Object LOCK = new Object();

    static int lockedSum(Shape sh, int n) {
        int t = 0;
        synchronized (LOCK) {
            for (int i = 0; i < n; i++) {
                t += sh.area() + i;
            }
        }
        return t;
    }

    static int lockedThrow(int[] a, int n) {
        int t = 0;
        synchronized (LOCK) {
            for (int i = 0; i < n; i++) {
                t += a[i];
            }
        }
        return t;
    }

    static final class P {
        final int x;
        final long y;

        P(int x, long y) {
            this.x = x;
            this.y = y;
        }
    }

    static long escapeOnRare(int i, boolean rare) {
        P p = new P(i, i * 3L);
        if (rare) {
            KEEP = p;
            return p.y - p.x;
        }
        return p.x + p.y;
    }

    static long loopEscape(int n, int rareAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            P p = new P(i, i * 5L);
            if (i == rareAt) {
                KEEP = p;
            }
            s += p.x + p.y;
        }
        return s;
    }

    static boolean otherThreadLocks() throws InterruptedException {
        final boolean[] got = {false};
        Thread t = new Thread(() -> {
            synchronized (LOCK) {
                got[0] = true;
            }
        });
        t.start();
        t.join(10_000);
        return got[0];
    }

    public static void main(String[] args) throws Exception {
        int[] five = {1, 2, 3, 4, 5};
        int[] dWarm = {1, 2, 4, 5, 10, 8};
        int[] dZero = {1, 2, 4, 5, 0, 8};
        long sink = 0;

        for (int r = 0; r < WARM; r++) {
            staticCells(five, 5);
            sink += divCells(dWarm, 6);
            fieldCells(new Acc(), five, 5);
            sink += elemCell(new long[1], five, 5);
            sink += freshCell(five, 5);
        }
        COUNT = 0;
        SUM = 0;
        try {
            staticCells(five, 7);
            fact("static-cells-throw", "none");
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("static-cells-throw", e);
        }
        fact("static-cells-count", COUNT);
        fact("static-cells-sum", SUM);

        COUNT = 0;
        try {
            sink += divCells(dZero, 6);
            fact("div-cells-throw", "none");
        } catch (ArithmeticException e) {
            thrown("div-cells-throw", e);
        }
        fact("div-cells-count", COUNT);

        Acc acc = new Acc();
        try {
            fieldCells(acc, five, 7);
            fact("field-cells-throw", "none");
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("field-cells-throw", e);
        }
        fact("field-cells-hits", acc.hits);
        fact("field-cells-total", acc.total);

        long[] cell = new long[1];
        try {
            sink += elemCell(cell, five, 7);
            fact("elem-cell-throw", "none");
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("elem-cell-throw", e);
        }
        fact("elem-cell-value", cell[0]);
        fact("fresh-cell-handler", freshCell(five, 7));

        byte[] sa = new byte[16];
        for (int i = 0; i < 16; i++) {
            sa[i] = (byte) ('a' + i);
        }
        for (int r = 0; r < WARM; r++) {
            sink += monotone(sa, 0, 16, new char[16], 0, 16);
            sink += divThenMonotone(sa, 16, new char[16], 16, r, 3);
            sink += down(five, 4);
            sink += neExit(five, r & 3);
            sink += upTo(five, 5);
        }
        try {
            fact("monotone-negative-start", monotone(sa, -1, 16, new char[16], 0, 16));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("monotone-negative-start", e);
        }
        char[] small = new char[8];
        try {
            fact("monotone-short-dest", monotone(sa, 0, 16, small, 0, 16));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("monotone-short-dest", e);
        }
        fact("monotone-short-dest-last", small[7]);
        char[] big = new char[32];
        try {
            fact("monotone-long-source", monotone(sa, 0, 20, big, 0, 32));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("monotone-long-source", e);
        }
        fact("monotone-long-source-last", big[15]);
        fact("monotone-early-return", monotone(sa, 0, 20, new char[4], 0, 4));
        try {
            fact("monotone-negative-dest", monotone(sa, 0, 16, new char[16], -2, 16));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("monotone-negative-dest", e);
        }
        fact("div-monotone-early-return", divThenMonotone(sa, 20, new char[4], 4, 84, 2));
        try {
            fact("div-monotone-zero", divThenMonotone(sa, 20, new char[4], 4, 84, 0));
        } catch (ArithmeticException e) {
            thrown("div-monotone-zero", e);
        }
        try {
            fact("down-past-end", down(five, 5));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("down-past-end", e);
        }
        fact("down-all", down(five, 4));
        fact("ne-exit-from-2", neExit(five, 2));
        try {
            fact("ne-exit-past-end", neExit(five, 6));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("ne-exit-past-end", e);
        }
        try {
            fact("ne-exit-negative", neExit(five, -1));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("ne-exit-negative", e);
        }
        COUNT = 0;
        try {
            fact("up-to-past-end", upTo(five, 9));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("up-to-past-end", e);
        }
        fact("up-to-past-end-count", COUNT);

        for (int r = 0; r < WARM; r++) {
            sink += tri(0, r & 31);
            sink += stride2(r & 7, 40);
            sink += ltri(r & 15, 64);
            sink += neWrap(0, r & 15);
            sink += inclUp(0, r & 15);
            sink += inclDown(r & 15, 0);
            sink += diamond(r, 1);
            evenOdd();
        }
        int triBad = 0;
        for (int n = 0; n <= 40; n++) {
            for (int start = -3; start <= 3; start++) {
                long want = 0;
                if (n > start) {
                    // Closed form of start + ... + (n - 1).
                    want = ((long) n * (n - 1) - (long) start * (start - 1)) / 2;
                }
                if (tri(start, n) != want) {
                    triBad++;
                }
            }
        }
        fact("tri-mismatches", triBad);
        fact("tri-near-max", tri(Integer.MAX_VALUE - 10, Integer.MAX_VALUE));
        fact("stride2-near-max", stride2(Integer.MAX_VALUE - 6, Integer.MAX_VALUE));
        fact("ltri-near-max", ltri(Long.MAX_VALUE - 9, Long.MAX_VALUE));
        fact("ne-wrap-count", neWrap(Integer.MAX_VALUE - 1, Integer.MIN_VALUE + 1));
        fact("ne-wrap-last-i", LAST_I);
        fact("ne-wrap-zero", neWrap(7, 7));
        fact("incl-up-near-max", inclUp(Integer.MAX_VALUE - 3, Integer.MAX_VALUE - 1));
        fact("incl-down-near-min", inclDown(Integer.MIN_VALUE + 2, Integer.MIN_VALUE + 1));
        fact("diamond", diamond(3, 10));
        EVEN = 0;
        ODD = 0;
        evenOdd();
        fact("even-odd", EVEN + "/" + ODD);

        Shape sq = new Sq(3);
        for (int r = 0; r < WARM; r++) {
            sink += lockedSum(sq, 10);
            sink += lockedThrow(five, 5);
            sink += escapeOnRare(r, false);
            sink += loopEscape(8, -1);
        }
        fact("lock-class-deopt-sum", lockedSum(new Rect(2, 5), 10));
        fact("lock-held-after-deopt", Thread.holdsLock(LOCK));
        try {
            fact("lock-throw", lockedThrow(five, 6));
        } catch (ArrayIndexOutOfBoundsException e) {
            thrown("lock-throw", e);
        }
        fact("lock-held-after-throw", Thread.holdsLock(LOCK));
        fact("lock-free-after-throw", otherThreadLocks());
        fact("rare-escape-value", escapeOnRare(7, true));
        P kept = (P) KEEP;
        fact("rare-escape-fields", kept.x + "/" + kept.y);
        KEEP = null;
        fact("loop-escape-value", loopEscape(8, 5));
        kept = (P) KEEP;
        fact("loop-escape-fields", kept.x + "/" + kept.y);
        if (sink == 42) {
            System.out.println("unlikely");
        }
    }
}
