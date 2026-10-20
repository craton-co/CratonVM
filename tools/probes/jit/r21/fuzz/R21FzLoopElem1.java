// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-1, the array-element accumulator
// promotion (`CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`,
// `loop_field_forward::promote_loop_array_elements`).
//
// Risk targeted: the pre-header that the promotion plants (`len = a.length`,
// `Guard(0 <= k)`, `Guard(k < len)`, the read `a[k]`) runs on EVERY loop
// entry, while the bytecode touches `a[k]` only from the first iteration on.
// So:
//   * a ZERO-TRIP loop with a bad index or a null array must not throw
//     (the guards must deopt to the header, never raise);
//   * a bad index with n > 0 must throw the AIOOBE / NPE at the first access,
//     with the iteration's EARLIER side effect (a static store, which the
//     census admits) already done exactly once;
//   * once the guards have trapped past the de-spec limit, the header is not
//     promoted again: answers unchanged;
//   * index edges: k = 0, k = len - 1, k = len, k = -1, k = MIN_VALUE.
// int and long elements, method-entry bodies (called hot) and the same rows
// after a deopt trigger (a class loaded late and a rare-branch exception).
//
// Run:
//   javac -d out R21FzLoopElem1.java
//   java -Xmx256m -cp out R21FzLoopElem1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopElem1                              # default
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21FzLoopElem1
//   cratonvm --compatible -Xmx256m -cp out R21FzLoopElem1
//   cratonvm --nojit -Xmx256m -cp out R21FzLoopElem1
// Engagement: CRATONVM_DBG_JITC=1 prints `[ir] loop array element promote: ...`;
//   CRATONVM_DBG_LOOP_FIELD_FORWARD=1 prints the refusals.
// HotSpot expectation: every line identical on every arm (no timing lines).

public class R21FzLoopElem1 {
    static final int WARM = 60_000;
    static int sSide;
    static long hash = 17;

    static void mix(long v) {
        hash = hash * 1_000_003L + v;
    }

    // ── kernels ──────────────────────────────────────────────────────────

    static int elemInt(int[] a, int k, int n, int x) {
        for (int i = 0; i < n; i++) {
            a[k] += i ^ x;
        }
        return n;
    }

    static long elemLong(long[] a, int k, int n, long x) {
        for (int i = 0; i < n; i++) {
            a[k] += i * x;
        }
        return n;
    }

    /** A static store BEFORE the element access in every iteration. */
    static int elemSide(int[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            sSide += 1;
            a[k] += i + 1;
        }
        return n;
    }

    /** The loop also reads a.length (admitted for the cell's own array). */
    static int elemLen(int[] a, int k, int n) {
        for (int i = 0; i < n; i++) {
            a[k] += a.length + i;
        }
        return n;
    }

    // ── rows ─────────────────────────────────────────────────────────────

    static String exc(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static String showI(int[] a) {
        return a == null ? "null" : java.util.Arrays.toString(a);
    }

    static String showL(long[] a) {
        return a == null ? "null" : java.util.Arrays.toString(a);
    }

    static void rowInt(String tag, int[] a, int k, int n, int x) {
        String r;
        try {
            elemInt(a, k, n, x);
            r = "ok";
        } catch (RuntimeException e) {
            r = exc(e);
        }
        System.out.println("row " + tag + " int k=" + k + " n=" + n + " -> " + r + " a=" + showI(a));
    }

    static void rowLong(String tag, long[] a, int k, int n, long x) {
        String r;
        try {
            elemLong(a, k, n, x);
            r = "ok";
        } catch (RuntimeException e) {
            r = exc(e);
        }
        System.out.println("row " + tag + " long k=" + k + " n=" + n + " -> " + r + " a=" + showL(a));
    }

    static void rowSide(String tag, int[] a, int k, int n) {
        int before = sSide;
        String r;
        try {
            elemSide(a, k, n);
            r = "ok";
        } catch (RuntimeException e) {
            r = exc(e);
        }
        System.out.println("row " + tag + " side k=" + k + " n=" + n + " -> " + r
                + " sideDelta=" + (sSide - before) + " a=" + showI(a));
    }

    static void rowLen(String tag, int[] a, int k, int n) {
        String r;
        try {
            elemLen(a, k, n);
            r = "ok";
        } catch (RuntimeException e) {
            r = exc(e);
        }
        System.out.println("row " + tag + " len k=" + k + " n=" + n + " -> " + r + " a=" + showI(a));
    }

    static void edges(String tag) {
        rowInt(tag, new int[4], 0, 10, 3);
        rowInt(tag, new int[4], 3, 10, 5);
        rowInt(tag, new int[4], 4, 0, 1);          // zero-trip, bad index: no throw
        rowInt(tag, new int[4], -1, 0, 1);         // zero-trip, negative index
        rowInt(tag, null, 0, 0, 1);                // zero-trip, null array
        rowInt(tag, new int[4], 4, 3, 1);          // AIOOBE on the first access
        rowInt(tag, new int[4], -1, 3, 1);
        rowInt(tag, new int[4], Integer.MIN_VALUE, 2, 1);
        rowInt(tag, null, 0, 2, 1);                // NPE on the first access
        rowInt(tag, new int[0], 0, 1, 1);
        rowInt(tag, new int[1], 0, 100_000, 7);    // long run, wraps
        rowLong(tag, new long[3], 2, 1000, 1L << 40);
        rowLong(tag, new long[3], 3, 0, 1);
        rowLong(tag, new long[3], 3, 1, 1);
        rowLong(tag, null, 1, 0, 1);
        rowSide(tag, new int[2], 1, 5);
        rowSide(tag, new int[2], 2, 5);            // sideDelta must be 1
        rowSide(tag, null, 0, 5);                  // sideDelta must be 1
        rowSide(tag, new int[2], -5, 0);           // sideDelta 0
        rowLen(tag, new int[6], 5, 9);
        rowLen(tag, new int[6], 6, 9);
    }

    static final class Late {
        int v = 3;
    }

    public static void main(String[] args) {
        edges("cold");
        int[] w = new int[8];
        long[] wl = new long[8];
        for (int r = 0; r < WARM; r++) {
            int k = r & 7;
            elemInt(w, k, 16 + (r & 3), r);
            elemLong(wl, k, 8, r);
            elemSide(w, 7 - k, 4);
            elemLen(w, k, 4);
            if ((r & 1023) == 0) {
                mix(w[k]);
                mix(wl[k]);
            }
        }
        System.out.println("warm w=" + showI(w) + " wl=" + showL(wl) + " side=" + sSide + " hash=" + hash);
        edges("hot");
        // Hammer the bad-index entries past the de-spec limit, then good again.
        int trapped = 0;
        for (int r = 0; r < 40; r++) {
            try {
                elemInt(new int[2], 2, 1, r);
            } catch (ArrayIndexOutOfBoundsException e) {
                trapped++;
            }
            elemInt(new int[2], 5, 0, r);
        }
        System.out.println("trapped " + trapped);
        for (int r = 0; r < WARM; r++) {
            elemInt(w, r & 7, 12, r);
        }
        System.out.println("rewarm w=" + showI(w));
        edges("despec");
        // Deopt triggers: a late class and a rare exception path.
        Late late = new Late();
        try {
            elemInt(w, late.v + 100, 1, 0);
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("late " + exc(e));
        }
        for (int r = 0; r < WARM / 4; r++) {
            elemInt(w, r & 7, 12, r);
            elemLong(wl, r & 7, 3, r);
        }
        System.out.println("after w=" + showI(w) + " wl=" + showL(wl));
        edges("after");
        System.out.println("done side=" + sSide);
    }
}
