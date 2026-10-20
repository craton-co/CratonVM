// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: what EA21-1 unlocks downstream. Folding
// `a == other` removes the identity observation that kept `a` on the heap
// (`ScalarRefusal::IdentityObserved`), so the NEXT escape-analysis round may
// scalar-replace it (`CRATONVM_JIT_EA_REF_COMPARE_FOLD`, the compile loop's
// `round_applied + ref_compare_folds`).
//
// Risk targeted: an object scalar-replaced only because its compare folded
// is then MATERIALIZED at a deopt. Two locals naming it (an alias, a phi of
// two such objects, a field of another replaced holder) must come back as
// ONE object: identity, identityHashCode, a monitor and a field store seen
// through the alias. Every hot method compares its allocation against a
// parameter (the fold) and has a branch never taken in the warm-up that
// reads the aliases; calling it with the rare value deopts there (an
// uncommon trap, or the interpreter if the tier compiled the branch).
// A second trigger loads a new implementation class of an interface the
// methods call on the fresh object's path.
//
// Run:
//   javac -d out R21Fz2EaCmp3.java
//   java -Xmx256m -cp out R21Fz2EaCmp3                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2EaCmp3
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2EaCmp3
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 CRATONVM_DBG_SCALAR_NEW=1 cratonvm ...   (stderr: which news were replaced)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2EaCmp3 {
    static final int WARM = 200_000;
    static final int RARE = 77_777;

    interface Op {
        int apply(int v);
    }

    static final class Inc implements Op {
        public int apply(int v) {
            return v + 1;
        }
    }

    static final class Late implements Op {
        public int apply(int v) {
            return v * 3;
        }
    }

    static final class P {
        int v;
        long w;

        P(int v) {
            this.v = v;
            this.w = v * 1_000_003L;
        }
    }

    static final class H {
        P f;
        int n;
    }

    static int m0(Object q, int x) {
        P a = new P(x);
        P alias = a;
        int r = a == q ? 100 : 0;
        if (x == RARE) {
            alias.v += 5;
            r += (alias == a ? 1 : 0) + (a.v == x + 5 ? 2 : 0)
                    + (System.identityHashCode(a) == System.identityHashCode(alias) ? 4 : 0);
        }
        return r + a.v;
    }

    static int m1(Object q, int x) {
        P a = new P(x);
        P b = new P(x + 1);
        P c = (x & 1) == 0 ? a : b;
        int r = (a == q ? 100 : 0) + (b == q ? 200 : 0);
        if (x == RARE || x == RARE + 1) {
            c.v = -1;
            r += (c == a ? 1 : 0) + (c == b ? 2 : 0) + (a.v == -1 ? 4 : 0) + (b.v == -1 ? 8 : 0);
        }
        return r + c.v;
    }

    static int m2(Object q, int x) {
        H h = new H();
        P a = new P(x);
        h.f = a;
        h.n = x;
        int r = (a == q ? 100 : 0) + (h == q ? 200 : 0);
        if (x == RARE) {
            a.w = 9;
            r += (h.f == a ? 1 : 0) + (h.f.w == 9 ? 2 : 0) + (h.n == x ? 4 : 0);
        }
        return r + h.f.v;
    }

    static int m3(Object q, int x) {
        P a = new P(x);
        P alias = a;
        int r = a != q ? 0 : 100;
        if (x == RARE) {
            synchronized (alias) {
                r += Thread.holdsLock(a) ? 1 : 0;
            }
            r += Thread.holdsLock(a) ? 2 : 0;
        }
        return r + (int) (a.w & 0xFF);
    }

    static int m4(Object q, Op op, int x) {
        P a = new P(x);
        P b = a;
        int r = a == q ? 100 : 0;
        int v = op.apply(a.v);
        if (v == RARE * 3) {
            r += b == a ? 1 : 0;
        }
        return r + v + b.v;
    }

    static int m5(Object q, int n) {
        P last = null;
        int hits = 0;
        for (int k = 0; k < n; k++) {
            P cur = new P(k);
            if (cur == q) {
                hits += 1000;
            }
            if (cur == last) {
                hits += 100;
            }
            if (k % 5 == 0) {
                last = cur;
            }
            if (last == cur) {
                hits++;
            }
        }
        return hits + (last == null ? -1 : last.v);
    }

    static String row(Object q, Op op, int x) {
        return m0(q, x) + " " + m1(q, x) + " " + m2(q, x) + " " + m3(q, x) + " " + m4(q, op, x) + " "
                + m5(q, x & 31);
    }

    public static void main(String[] args) {
        Object q = new P(3);
        Op inc = new Inc();
        int[] xs = {0, 1, 2, 3, 30, 31, -7};
        for (int x : xs) {
            System.out.println("row cold x=" + x + " | " + row(q, inc, x));
        }
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int x = r & 1023;
            h = h * 31 + m0(q, x) + m1(q, x) + m2(q, x) + m3(q, x) + m4(q, inc, x) + m5(q, x & 15);
        }
        System.out.println("warm hash " + h);
        for (int x : xs) {
            System.out.println("row hot x=" + x + " | " + row(q, inc, x));
        }
        // Deopt trigger 1: the rare branches.
        System.out.println("rare m0=" + m0(q, RARE) + " m1=" + m1(q, RARE) + "/" + m1(q, RARE + 1) + " m2="
                + m2(q, RARE) + " m3=" + m3(q, RARE));
        // Deopt trigger 2: a new Op implementation at m4's call site.
        Op late = new Late();
        System.out.println("late m4=" + m4(q, late, RARE) + " " + m4(q, late, 4) + " " + m4(q, inc, RARE * 3 - 1));
        for (int x : xs) {
            System.out.println("row after x=" + x + " | " + row(q, late, x));
        }
        // Rare again, after re-warming.
        for (int r = 0; r < WARM / 4; r++) {
            h = h * 31 + m0(q, r & 1023) + m1(q, r & 1023) + m2(q, r & 1023) + m3(q, r & 1023);
        }
        System.out.println("rewarm hash " + h);
        System.out.println("rare2 m0=" + m0(q, RARE) + " m1=" + m1(q, RARE) + "/" + m1(q, RARE + 1) + " m2="
                + m2(q, RARE) + " m3=" + m3(q, RARE));
    }
}
