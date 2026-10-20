// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 13: residual 1 of
// docs/internal/fixed-bugs/r21-finalize-compiled-ctor-throw-keeps-registration-FIXED-20261019.md.
// A NON-finalizable ancestor constructor that can throw before its initializing call is compiled
// hot BEFORE its finalizable subclass is loaded; then the subclass is constructed hot through it.
// HotSpot registers a finalizer only when Object.<init> returns, so an object whose ancestor
// constructor threw before that point is never finalized.
// Switch: CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED (default ON; =0 is the control).
//
// Run:
//   javac -d out R21FinalizeAncestorCtor.java
//   java -Xmx256m -cp out R21FinalizeAncestorCtor          # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FinalizeAncestorCtor      (and --compatible, --nojit)
// HotSpot expectation: `throws 20000 finalized 0`, `normal 20000 finalized 20000`.

import java.util.concurrent.atomic.AtomicInteger;

public class R21FinalizeAncestorCtor {
    static final int N = 20_000;
    static final AtomicInteger FIN = new AtomicInteger();

    static class Base {
        final int v;

        Base(String s) {
            this(Integer.parseInt(s));
        }

        Base(int v) {
            this.v = v;
        }
    }

    // Loaded only after Base.<init>(String) is hot (first touched in phase 2).
    static class Fin extends Base {
        Fin(String s) {
            super(s);
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FIN.incrementAndGet();
        }
    }

    static void collect() {
        System.gc();
        System.runFinalization();
        try {
            Thread.sleep(2);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static int settle() {
        int last = -1;
        int still = 0;
        for (int i = 0; i < 400; i++) {
            collect();
            int now = FIN.get();
            if (now == last) {
                still++;
            } else {
                still = 0;
                last = now;
            }
            if (i >= 20 && still >= 10) {
                break;
            }
        }
        return FIN.get();
    }

    static long warm() {
        long s = 0;
        for (int r = 0; r < 200_000; r++) {
            s += new Base(Integer.toString(r & 1023)).v;
        }
        return s;
    }

    static Object make(String s) {
        return new Fin(s);
    }

    public static void main(String[] args) {
        System.out.println("warm " + warm());
        settle();
        FIN.set(0);
        int thrown = 0;
        for (int r = 0; r < N; r++) {
            try {
                make("x" + r);
            } catch (NumberFormatException e) {
                thrown++;
            }
        }
        System.out.println("throws " + thrown + " finalized " + settle());
        FIN.set(0);
        int made = 0;
        for (int r = 0; r < N; r++) {
            if (make(Integer.toString(r)) != null) {
                made++;
            }
        }
        System.out.println("normal " + made + " finalized " + settle());
    }
}
