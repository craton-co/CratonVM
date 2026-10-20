// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: EA21-1, the confined reference-compare
// fold (`CRATONVM_JIT_EA_REF_COMPARE_FOLD`;
// `escape_analysis::find_confined_ref_compare_folds` +
// `ea_ir_bridge::ir_fold_confined_ref_compares`).
//
// Risk targeted: the fold turns `a == b` into `0` (and `!=` into `1`) when
// `a` names one NoEscape allocation and `b`'s points-to set lacks it. Every
// row below has a route by which `b` CAN be `a` -- a confined holder's
// field, a confined array slot read by another (run-time equal) index, a
// phi with the allocation on one arm, a loop that saves the current object,
// a callee returning its argument, a checkcast copy, a thrown-and-caught
// exception, a lambda capture, clone / arraycopy, collections, Optional,
// WeakReference, Object.equals / Objects.equals (spliced `this == obj`) --
// so a fold that misses the route prints a wrong 0. Rows that really are
// never-equal (two fresh objects, fresh vs a parameter / null) pin the
// answers the fold may give. Cold, hot (200k trips), after a deopt trigger
// (rare branches, a new receiver class), again.
//
// Run:
//   javac -d out R21Fz2EaCmp1.java
//   java -Xmx256m -cp out R21Fz2EaCmp1                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2EaCmp1
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2EaCmp1
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 \
//     CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2EaCmp1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.Objects;
import java.util.Optional;
import java.util.function.Supplier;

public class R21Fz2EaCmp1 {
    static final int WARM = 200_000;

    static final class P {
        int v;

        P(int v) {
            this.v = v;
        }
    }

    static final class H {
        Object f;
    }

    static Object sink;

    static int r0(Object o) {
        P a = new P(1);
        return (a == o ? 1 : 0) + (a != o ? 2 : 0) + a.v;
    }

    static int r1(int x) {
        P a = new P(x);
        P b = new P(x);
        return (a == b ? 100 : 0) + (a != b ? 10 : 0) + a.v + b.v;
    }

    static int r2(int x) {
        P a = new P(x);
        Object n = (x & 1) == 0 ? null : a;
        return (a == n ? 1 : 0) + (n == null ? 2 : 0) + (a == null ? 4 : 0);
    }

    static int r3(int x) {
        H h = new H();
        P a = new P(x);
        h.f = a;
        Object back = h.f;
        return back == a ? 1 : 0;
    }

    static int r4(int x) {
        H h = new H();
        P a = new P(x);
        P b = new P(x + 1);
        h.f = (x & 1) == 0 ? a : b;
        return (h.f == a ? 1 : 0) + (h.f == b ? 2 : 0);
    }

    static int r5(int x, int i, int j) {
        Object[] arr = new Object[4];
        P a = new P(x);
        arr[i & 3] = a;
        return (arr[j & 3] == a ? 1 : 0) + (arr[(j + 1) & 3] == a ? 2 : 0);
    }

    static int r6(int n) {
        Object prev = null;
        int eq = 0;
        for (int k = 0; k < n; k++) {
            P cur = new P(k);
            if (prev == cur) {
                eq += 100;
            }
            if (k == 3) {
                prev = cur;
            }
            if (prev == cur) {
                eq++;
            }
        }
        return eq;
    }

    static int r7(int x) {
        P a = new P(x);
        P b = new P(x);
        return (a.equals(a) ? 1 : 0) + (a.equals(b) ? 2 : 0) + (Objects.equals(a, a) ? 4 : 0)
                + (Objects.equals(a, b) ? 8 : 0) + (Objects.equals(a, null) ? 16 : 0);
    }

    static Object id(Object o) {
        return o;
    }

    static int r8(int x) {
        P a = new P(x);
        return id(a) == a ? 1 : 0;
    }

    static int r9(int x) {
        P a = new P(x);
        sink = a;
        return sink == a ? 1 : 0;
    }

    static int r10(int x) {
        Object o = new P(x);
        P p = (P) o;
        return p == o ? 1 : 0;
    }

    static int r11(Object q, int x) {
        P a = new P(x);
        Object m = (x & 1) == 0 ? a : q;
        return (m == a ? 1 : 0) + (m == q ? 2 : 0);
    }

    static int r12(int x) {
        P a = new P(x);
        P b;
        synchronized (a) {
            b = x > 1_000_000 ? new P(0) : a;
        }
        return a == b ? 1 : 0;
    }

    static int r13(int n) {
        H h = new H();
        int c = 0;
        for (int k = 0; k < n; k++) {
            P cur = new P(k);
            if (h.f == null) {
                h.f = cur;
            }
            if (h.f == cur) {
                c++;
            }
        }
        return c;
    }

    static int r14(int x) {
        RuntimeException ex = new IllegalStateException("s");
        try {
            if (x >= 0) {
                throw ex;
            }
        } catch (IllegalStateException e) {
            return e == ex ? 1 : 2;
        }
        return 3;
    }

    static int r15(int x) {
        P a = new P(x);
        Supplier<Object> s = () -> a;
        return s.get() == a ? 1 : 0;
    }

    static int r16(int x) {
        P a = new P(x);
        Object[] arr = {a, null};
        Object[] c = arr.clone();
        return (c[0] == a ? 1 : 0) + (c == arr ? 2 : 0) + (c[1] == a ? 4 : 0);
    }

    static int r17(int x) {
        P a = new P(x);
        Object[] arr = {a, null};
        System.arraycopy(arr, 0, arr, 1, 1);
        return arr[1] == a ? 1 : 0;
    }

    static int r18(int x) {
        P a = new P(x);
        ArrayList<Object> l = new ArrayList<>();
        l.add(a);
        return (l.get(0) == a ? 1 : 0) + (l.contains(a) ? 2 : 0) + (l.indexOf(new P(x)) == -1 ? 4 : 0);
    }

    static int r19(int x) {
        P a = new P(x);
        return Optional.of(a).get() == a ? 1 : 0;
    }

    static int r20(int x) {
        P a = new P(x);
        WeakReference<P> w = new WeakReference<>(a);
        int r = w.get() == a ? 1 : 0;
        return r + (a.v == x ? 2 : 0);
    }

    static int r21(int x) {
        P a = new P(x);
        IdentityHashMap<Object, Integer> m = new IdentityHashMap<>();
        m.put(a, 1);
        return (m.containsKey(a) ? 1 : 0) + (m.containsKey(new P(x)) ? 2 : 0);
    }

    static int r22(int x) {
        P a = new P(x);
        HashMap<Object, Integer> m = new HashMap<>();
        m.put(a, 7);
        Integer got = m.get(a);
        return got == null ? -1 : got;
    }

    static int r23(int x) {
        P a = new P(x);
        P[] one = {a};
        P[] two = {new P(x)};
        return (one[0] == a ? 1 : 0) + (two[0] == a ? 2 : 0) + (one[0] == two[0] ? 4 : 0);
    }

    static String row(int x, Object q) {
        int[] v = {
            r0(q), r1(x), r2(x), r3(x), r4(x), r5(x, x, x), r5(x, x, x + 1), r6(x & 15), r7(x), r8(x), r9(x),
            r10(x), r11(q, x), r12(x), r13(x & 7), r14(x), r15(x), r16(x), r17(x), r18(x), r19(x), r20(x),
            r21(x), r22(x), r23(x),
        };
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < v.length; i++) {
            sb.append(i == 0 ? "" : " ").append(v[i]);
        }
        return sb.toString();
    }

    static void rows(String tag, Object q) {
        int[] xs = {0, 1, 2, 3, 4, 5, 6, 7, 8, 15, 16, 31, -1, -2};
        for (int x : xs) {
            System.out.println("row " + tag + " x=" + x + " | " + row(x, q));
        }
    }

    public static void main(String[] args) {
        Object q = new P(5);
        rows("cold", q);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int x = r & 15;
            h = h * 31 + r0(q) + r1(x) + r2(x) + r3(x) + r4(x) + r5(x, r, r) + r5(x, r, r + 1);
            h = h * 31 + r6(x) + r7(x) + r8(x) + r9(x) + r10(x) + r11(q, x) + r12(x) + r13(x & 7);
            h = h * 31 + r14(x) + r15(x) + r16(x) + r17(x) + r23(x);
            if ((r & 7) == 0) {
                h = h * 31 + r18(x) + r19(x) + r20(x) + r21(x) + r22(x);
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", q);
        // Deopt triggers: a new receiver class through the Object parameter,
        // the never-taken arms (x < 0 in r14, x > 1e6 in r12), a long r6 loop.
        Object late = new Object() {
            @Override
            public String toString() {
                return "late";
            }
        };
        rows("late", late);
        System.out.println("rare r12=" + r12(2_000_000) + " r14=" + r14(-9) + " r6=" + r6(1000)
                + " r13=" + r13(1000) + " r0null=" + r0(null) + " r11null=" + r11(null, 3));
        rows("after", q);
    }
}
