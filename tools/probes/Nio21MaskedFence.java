// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 7, lane L5: small counted loops over a constant-masked index,
// `a[(i + k) & m]` -- the `R17NioRows fence` / `Nio20FenceRow own` shape and
// its neighbours. Since this wave three switches are default ON:
//   CRATONVM_JIT_IR_MASK_GUARD_ELIDE        the lowering drops the null and
//                                          bounds checks a pre-header
//                                          `m < a.length` guard proves, and
//                                          `node_cannot_deopt` claims the load
//   CRATONVM_JIT_IR_MASK_PREDICATE_EARLY    that guard is planted BEFORE the
//                                          loop rounds, for a short top-level
//                                          constant-trip loop whose only array
//                                          access is a fenced masked load
//   CRATONVM_JIT_IR_UNROLL_DROP_FENCED_LOAD the full unroller retires such a
//                                          load with its reachability fence
// Each `=0` arm (and all three together) must print the same `conf` text.
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): each
//          kernel over arrays of length 16, 8, 7, 1, 0 and null, the result
//          or the exception class (and, in the cold pass, its message, except
//          for an NPE, whose helpful message names javac's locals), cold, after a warm-up on
//          valid arrays (the compiled loops then carry the entry guard and
//          the short arrays trap it), and again after the failing cases ran
//          enough times to withdraw the guard (the de-spec registry).
//   rows   `<row> <N> ns/op` lines:
//          own      the fence loop over a static final table (Nio20FenceRow own)
//          param    the same loop over a parameter array
//          sum      `h = h * 31 + t[(i + k) & 15]`: the load feeds a carried
//                   value (not dropped; the late predicate only)
//          nested   the fence loop nested in a runtime-bound loop (no early
//                   plant: an outer loop's LICM would see the guard)
//
//   java Nio21MaskedFence conf
//   java -Dnio21.rows=own,param,sum,nested Nio21MaskedFence rows

import java.lang.ref.Reference;

public class Nio21MaskedFence {
    static final int HOT = 40_000;
    static volatile long sink;

    static final Object[] PLAIN = {
        "a", "b", "c", "d", "e", "f", "g", "h",
    };

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "own,param,sum,nested"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- kernels

    static int fenceStatic(int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = PLAIN[(i + k) & 7];
            h += k ^ i;
            Reference.reachabilityFence(o);
        }
        return h;
    }

    static int fenceParam(Object[] a, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            Object o = a[(i + k) & 7];
            h += k ^ i;
            Reference.reachabilityFence(o);
        }
        return h;
    }

    static int maskedSum(int[] t, int i) {
        int h = 1;
        for (int k = 0; k < 16; k++) {
            h = h * 31 + t[(i + k) & 15];
        }
        return h;
    }

    static int nested(Object[] a, int n) {
        int h = 0;
        for (int j = 0; j < n; j++) {
            for (int k = 0; k < 4; k++) {
                Object o = a[(j + k) & 7];
                h += j ^ k;
                Reference.reachabilityFence(o);
            }
        }
        return h;
    }

    // --------------------------------------------------------------- conf

    static boolean fullMessages = true;

    static String describe(Throwable t) {
        // An NPE's helpful message names javac's locals; and after warm-up HotSpot's C2
        // throws its preallocated implicit exceptions (OmitStackTraceInFastThrow) with no
        // message, so the warm passes print the class alone.
        if (t instanceof NullPointerException || !fullMessages) {
            return t.getClass().getName();
        }
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static Object[] objs(int n) {
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) {
            a[i] = Integer.valueOf(i * 3 + 1);
        }
        return a;
    }

    static int[] ints(int n) {
        int[] t = new int[n];
        for (int i = 0; i < n; i++) {
            t[i] = i * i - 7;
        }
        return t;
    }

    static final int[] LENGTHS = {16, 8, 7, 1, 0, -1};

    static void table(String pass) {
        System.out.println("== " + pass);
        System.out.println("static i=5 = " + fenceStatic(5));
        for (int len : LENGTHS) {
            Object[] a = len < 0 ? null : objs(len);
            int[] t = len < 0 ? null : ints(len);
            for (int i : new int[] {0, 3, -9}) {
                String r;
                try {
                    r = "= " + fenceParam(a, i);
                } catch (Throwable e) {
                    r = "! " + describe(e);
                }
                System.out.println("param len=" + len + " i=" + i + " " + r);
                try {
                    r = "= " + maskedSum(t, i);
                } catch (Throwable e) {
                    r = "! " + describe(e);
                }
                System.out.println("sum len=" + len + " i=" + i + " " + r);
            }
            for (int n : new int[] {0, 3, 9}) {
                String r;
                try {
                    r = "= " + nested(a, n);
                } catch (Throwable e) {
                    r = "! " + describe(e);
                }
                System.out.println("nested len=" + len + " n=" + n + " " + r);
            }
        }
    }

    static void warm() {
        Object[] a8 = objs(8);
        Object[] a16 = objs(16);
        int[] t16 = ints(16);
        long acc = 0;
        for (int r = 0; r < HOT; r++) {
            acc += fenceStatic(r);
            acc += fenceParam((r & 1) == 0 ? a8 : a16, r);
            acc += maskedSum(t16, r);
            acc += nested(a8, r & 15);
        }
        sink = acc;
    }

    static void conf() {
        table("cold");
        warm();
        fullMessages = false;
        table("warm");
        // Enough failing entries to exhaust the per-bci de-spec limit, then a
        // recompile on valid arrays: the guard is withdrawn, the checks stay.
        Object[] a1 = objs(1);
        int[] t1 = ints(1);
        for (int r = 0; r < 64; r++) {
            try {
                fenceParam(a1, r);
            } catch (ArrayIndexOutOfBoundsException e) {
                sink += 1;
            }
            try {
                maskedSum(t1, r);
            } catch (ArrayIndexOutOfBoundsException e) {
                sink += 1;
            }
        }
        warm();
        table("despec");
    }

    // --------------------------------------------------------------- rows

    static void rows(String which) {
        warm();
        Object[] a8 = objs(8);
        int[] t16 = ints(16);
        int ops = Integer.getInteger("nio21.ops", 5_000_000);
        for (String row : which.split(",")) {
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long acc = 0;
                long t0 = System.nanoTime();
                switch (row) {
                    case "own":
                        for (int i = 0; i < ops; i++) acc += fenceStatic(i);
                        break;
                    case "param":
                        for (int i = 0; i < ops; i++) acc += fenceParam(a8, i);
                        break;
                    case "sum":
                        for (int i = 0; i < ops; i++) acc += maskedSum(t16, i);
                        break;
                    case "nested":
                        for (int i = 0; i < ops; i++) acc += nested(a8, 4);
                        break;
                    default:
                        System.out.println(row + " unknown");
                        return;
                }
                long dt = System.nanoTime() - t0;
                sink = acc;
                best = Math.min(best, dt);
            }
            System.out.println(row + " " + (best / ops) + " ns/op");
        }
    }
}
