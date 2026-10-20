// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: EA21-1 (`CRATONVM_JIT_EA_REF_COMPARE_FOLD`)
// next to the monitor paths escape analysis also owns (lock elision on a
// confined object, `find_lock_elision_plans`), and identity observers that
// are not `acmp`: identityHashCode, Object.hashCode / toString, wait /
// notify, Thread.holdsLock.
//
// Risk targeted: once `a == other` folds, a confined `a` may lose its
// monitor (elided) and its heap copy (replaced). Its identity must still
// answer consistently wherever the program can observe it:
// `System.identityHashCode(a)` twice, `a.hashCode()` vs identityHashCode,
// `a.toString()` (class@hash) parsed back to the hash, `Thread.holdsLock(a)`
// inside and outside `synchronized (a)`, nested re-entrant locking,
// `a.wait(0)` / `notify()` outside the monitor
// (IllegalMonitorStateException with HotSpot's text), `wait(1)` inside it,
// and a compare of `a` against itself through a synchronized alias. Rows
// print booleans and exception texts only (no hash values). Cold, hot
// (200k), a deopt trigger (rare branch with the monitor held), again.
//
// Run:
//   javac -d out R21Fz2EaLock1.java
//   java -Xmx256m -cp out R21Fz2EaLock1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2EaLock1
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2EaLock1
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 CRATONVM_JIT_LOCK_COARSEN=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2EaLock1 {
    static final int WARM = 200_000;
    static final int RARE = 4_444_444;

    static final class P {
        int v;

        P(int v) {
            this.v = v;
        }
    }

    static int k0(Object q, int x) {
        P a = new P(x);
        int r = a == q ? 100 : 0;
        int h1 = System.identityHashCode(a);
        int h2 = System.identityHashCode(a);
        int h3 = a.hashCode();
        String s = a.toString();
        int at = s.indexOf('@');
        boolean strOk = Integer.toHexString(h1).equals(s.substring(at + 1));
        return r + (h1 == h2 ? 1 : 0) + (h1 == h3 ? 2 : 0) + (strOk ? 4 : 0);
    }

    static int k1(Object q, int x) {
        P a = new P(x);
        int r = a == q ? 100 : 0;
        boolean before = Thread.holdsLock(a);
        boolean inside;
        boolean nested;
        synchronized (a) {
            inside = Thread.holdsLock(a);
            synchronized (a) {
                nested = Thread.holdsLock(a);
                a.v++;
            }
        }
        boolean after = Thread.holdsLock(a);
        return r + (before ? 1 : 0) + (inside ? 2 : 0) + (nested ? 4 : 0) + (after ? 8 : 0) + (a.v == x + 1 ? 16 : 0);
    }

    static String k2(Object q, int x) {
        P a = new P(x);
        String r = a == q ? "eq " : "";
        try {
            a.notify();
            r += "notified ";
        } catch (IllegalMonitorStateException e) {
            r += "IMSE(" + e.getMessage() + ") ";
        }
        try {
            a.wait(1);
            r += "waited ";
        } catch (IllegalMonitorStateException e) {
            r += "IMSE(" + e.getMessage() + ") ";
        } catch (InterruptedException e) {
            r += "INT ";
        }
        synchronized (a) {
            a.notifyAll();
            r += "inside ";
        }
        return r;
    }

    static int k3(Object q, int x) {
        P a = new P(x);
        Object alias = a;
        int r = a == q ? 100 : 0;
        synchronized (alias) {
            r += alias == a ? 1 : 0;
            if (x == RARE) {
                r += Thread.holdsLock(a) ? 2 : 0;
                r += System.identityHashCode(alias) == System.identityHashCode(a) ? 4 : 0;
            }
        }
        return r;
    }

    static String row(Object q, int x) {
        return k0(q, x) + " " + k1(q, x) + " " + k2(q, x) + " " + k3(q, x);
    }

    public static void main(String[] args) throws Exception {
        Object q = new P(1);
        System.out.println("row cold | " + row(q, 1) + " | " + row(null, 2));
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + k0(q, r) + k1(q, r) + k3(q, r);
            if ((r & 1023) == 0) {
                h = h * 31 + k2(q, r).length();
            }
        }
        System.out.println("warm hash " + h);
        System.out.println("row hot | " + row(q, 1) + " | " + row(null, 2));
        System.out.println("rare " + k3(q, RARE) + " " + k3(null, RARE));
        System.out.println("row after | " + row(q, 3) + " | " + row(new Object(), 4));
    }
}
