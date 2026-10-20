// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: the deopt locals backstop's false-positive
// side (`CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP`, default ON:
// `deopt_resume::cat2_pair_split_in_locals` drops a `Long` / `Double` whose
// upper-half slot holds a DESCRIBED value and keeps that value instead).
//
// Risk targeted: the backstop fires on any producer that describes a live
// cat-2 value's upper half as something other than `Undefined` -- a
// whole-method kind table that calls the slot `Int` because javac REUSES it
// for an int in a later scope, an OSR snapshot, a spliced callee's frame, a
// handler frame. Then a LIVE long is replaced by `Int(0)` at the deopt and
// the method continues with 0. These are ordinary javac methods: scopes that
// reuse a long's slots for ints and refs later (and earlier), doubles and
// longs interleaved with ints, a long live across a loop with a deopt in the
// loop (OSR too), a long parameter, an exception handler that reads a long,
// a spliced callee holding longs. Each deopts after the warm-up (a new
// receiver class, a rare branch, an exception) while the longs are live,
// and prints every long exactly.
//
// Run:
//   javac -d out R21Fz2Cat2Deopt1.java
//   java -Xmx256m -cp out R21Fz2Cat2Deopt1                               # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2Cat2Deopt1
//   CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP=0 cratonvm -Xmx256m -cp out R21Fz2Cat2Deopt1   (kill: no false positive possible)
//   CRATONVM_DBG_DEOPT=1 cratonvm ... (stderr must NOT show "treated as TOP" for this javac code)
//   CRATONVM_C2_ACCEPT=never cratonvm ... (single-pass frames only)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2Cat2Deopt1 {
    static final int WARM = 200_000;
    static final int RARE = 31_337;

    interface Step {
        long next(long v);
    }

    static final class Add implements Step {
        public long next(long v) {
            return v + 3;
        }
    }

    static final class Mul implements Step {
        public long next(long v) {
            return v * 5;
        }
    }

    // javac: {long a (1-2)} then {int x (1), int y (2)} then {long b (1-2)}.
    static long scopes(int c, Step st) {
        long out = 0;
        {
            long a = c * 1_000_000_007L;
            out += st.next(a);
        }
        {
            int x = c + 1;
            int y = c * 2;
            out += x * 31 + y;
        }
        {
            long b = -c * 77L;
            double d = c / 3.0;
            int z = c ^ 0x55;
            out += st.next(b) + (long) (d * 1000) + z;
        }
        return out;
    }

    static long interleaved(int c, Step st) {
        int i1 = c;
        long l1 = c * 3L;
        int i2 = c + 7;
        double d1 = c * 0.5;
        int i3 = -c;
        long l2 = Long.MAX_VALUE - c;
        Object o = (c & 1) == 0 ? "even" : null;
        long r = st.next(l1);
        if (c == RARE) {
            r += 1_000_000;
        }
        return r + i1 + i2 + i3 + (long) d1 + (l2 >>> 33) + (o == null ? 1 : 2);
    }

    static long loop(int n, Step st) {
        long acc = 1;
        double dacc = 0.25;
        int k = 0;
        for (int i = 0; i < n; i++) {
            long t = st.next(i);
            acc = acc * 31 + t;
            dacc += t * 0.5;
            k += i & 3;
        }
        return acc + (long) dacc + k;
    }

    static long param(long p, int c, double q, Step st) {
        long r = st.next(p);
        int w = c + 1;
        if (c == RARE) {
            r += w;
        }
        return r + w + (long) q;
    }

    static long handler(int c, int[] arr, Step st) {
        long before = c * 11L;
        double dd = c + 0.125;
        try {
            before += arr[c & 7];
            before = st.next(before);
        } catch (ArrayIndexOutOfBoundsException e) {
            return before * 2 + (long) (dd * 8);
        }
        return before + (long) (dd * 8);
    }

    static long inner(long a, long b, Step st) {
        long m = a ^ b;
        return st.next(m) + a - b;
    }

    static long outer(int c, Step st) {
        long x = c * 13L;
        int y = c + 5;
        long z = inner(x, y, st);
        double w = z / 2.0;
        return z + y + (long) w;
    }

    static String row(int c, Step st) {
        return scopes(c, st) + " " + interleaved(c, st) + " " + loop(c & 63, st) + " "
                + param(c * 7L, c, c * 1.5, st) + " " + handler(c, new int[c & 7], st) + " " + outer(c, st);
    }

    public static void main(String[] args) {
        Step add = new Add();
        int[] cs = {0, 1, 2, 7, 8, 63, 1000, -5};
        for (int c : cs) {
            System.out.println("row cold c=" + c + " | " + row(c, add));
        }
        long h = 0;
        int[] arr = new int[8];
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + scopes(r, add) + interleaved(r & 0xFFF, add) + param(r, r & 0xFFF, r, add)
                    + handler(r, arr, add) + outer(r, add);
            if ((r & 63) == 0) {
                h = h * 31 + loop(r & 255, add);
            }
        }
        System.out.println("warm hash " + h);
        for (int c : cs) {
            System.out.println("row hot c=" + c + " | " + row(c, add));
        }
        // Deopt triggers: the rare value, an AIOOBE in handler, and a new
        // receiver class at every call site; an OSR loop that meets it mid-way.
        System.out.println("rare " + interleaved(RARE, add) + " " + param(5, RARE, 2.5, add) + " "
                + handler(3, new int[1], add));
        Step mul = new Mul();
        for (int c : cs) {
            System.out.println("row late c=" + c + " | " + row(c, mul));
        }
        Step[] mixed = {add, add, add, mul};
        long osr = 0;
        for (int i = 0; i < 3_000_000; i++) {
            long v = mixed[i == 2_000_000 ? 3 : 0].next(i);
            osr = osr * 31 + v;
        }
        System.out.println("osr " + osr + " " + loop(1_000_000, add));
        Step lam = v -> v - 1;
        for (int c : cs) {
            System.out.println("row after c=" + c + " | " + row(c, lam));
        }
    }
}
