// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: the finalizer compile bar for a
// non-finalizable ANCESTOR whose constructor throws before Object.<init>
// (`CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`, wave 13), when the
// finalizable class is loaded only TRANSITIVELY, as the superclass of the
// class the program names.
//
// Risk targeted (item 1 of
// docs/known-issues/jit/r21-review-w16-findings-20261019.md):
// `exception_dispatch::finalizable_define_stale_ancestors` stops its walk at
// the first finalizable superclass, on the assumption that that class's own
// define already recorded the classes above it. The VM-side listener
// (`SharedVm::note_finalizable_define`) runs only for the class a load
// request named, so a finalizable class loaded as a superclass inside the
// class manager is never listened to. Here `TLeaf extends TFin extends TBase`:
// the program names `TLeaf` only, `TFin` is the finalizable one, and
// `TBase(String)` is `this(Integer.parseInt(s))`, warmed (compiled) on plain
// `TBase` objects before `TLeaf` loads. A NumberFormatException thrown by the
// compiled `TBase(String)` body for a `TLeaf` object is a throw before
// Object.<init>: HotSpot (RegisterFinalizersAtInit) never registers such an
// object, so `uncon` must be 0. The `C*` chain is the control: there the
// program names the finalizable class itself, so the listener runs for it.
//
// Every `TLeaf` / `CFin` object that finishes construction sets `ok` before it
// is dropped, so its finalizer counts `con`. One that did not finish counts
// `uncon`. The settle loop waits for every constructed object to be finalized
// (bounded), then runs a few more rounds so a late unconstructed one shows.
//
// Passes: `warm` (60k plain base constructions per chain, half of them
// throwing, plus one long OSR call), `leaf` (the finalizable objects, 3000 per
// chain, every third string invalid), `leaf2` (the same after the leaf
// constructors are hot).
//
// Run:
//   javac -d out R21Fz5FinTransitive1.java
//   java -Xmx256m -cp out R21Fz5FinTransitive1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5FinTransitive1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5FinTransitive1    # VM-native app loads
//   cratonvm --nojit -Xmx256m -cp out R21Fz5FinTransitive1
//   CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED=0 cratonvm ...   # control arm
// HotSpot expectation: every line identical on every arm, `uncon=0`
// everywhere. javac warns that finalize() is deprecated for removal; that
// warning is expected.

import java.util.concurrent.atomic.AtomicInteger;

public class R21Fz5FinTransitive1 {
    static final int WARM = 60_000;
    static final int N = 3000;

    // Index 0: the transitive chain (T*), 1: the control chain (C*).
    static final AtomicInteger[] CON = {new AtomicInteger(), new AtomicInteger()};
    static final AtomicInteger[] UNCON = {new AtomicInteger(), new AtomicInteger()};

    static void fin(boolean ok, int chain) {
        if (ok) {
            CON[chain].incrementAndGet();
        } else {
            UNCON[chain].incrementAndGet();
        }
    }

    static String arg(int i) {
        return (i % 3 == 0) ? ("x" + i) : Integer.toString(i % 1000);
    }

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static void settle(int chain, int expected) {
        for (int round = 0; round < 400 && CON[chain].get() < expected; round++) {
            System.gc();
            System.runFinalization();
            pause(5);
        }
        for (int round = 0; round < 4; round++) {
            System.gc();
            System.runFinalization();
            pause(2);
        }
    }

    // Plain base objects: never finalizable. Warms TBase(String) / CBase(String).
    static long warmT(int n) {
        long h = 0;
        int nfe = 0;
        for (int i = 0; i < n; i++) {
            try {
                h = h * 31 + new R21Fz5TBase(arg(i)).v;
            } catch (NumberFormatException e) {
                nfe++;
            }
        }
        return h * 7 + nfe;
    }

    static long warmC(int n) {
        long h = 0;
        int nfe = 0;
        for (int i = 0; i < n; i++) {
            try {
                h = h * 31 + new R21Fz5CBase(arg(i)).v;
            } catch (NumberFormatException e) {
                nfe++;
            }
        }
        return h * 7 + nfe;
    }

    // The first mention of R21Fz5TLeaf in the program: loading it loads
    // R21Fz5TFin (finalizable) as its superclass.
    static int leafT(int n) {
        int built = 0;
        for (int i = 0; i < n; i++) {
            try {
                R21Fz5TLeaf leaf = new R21Fz5TLeaf(arg(i));
                built += (leaf.v >= 0) ? 1 : 0;
            } catch (NumberFormatException e) {
                // the unconstructed object: HotSpot never registered it
            }
        }
        return built;
    }

    static int leafC(int n) {
        int built = 0;
        for (int i = 0; i < n; i++) {
            try {
                R21Fz5CFin obj = new R21Fz5CFin(arg(i));
                built += (obj.v >= 0) ? 1 : 0;
            } catch (NumberFormatException e) {
                // unconstructed
            }
        }
        return built;
    }

    static void report(String pass, int chain, int built, int expectedTotal) {
        settle(chain, expectedTotal);
        System.out.println(pass + " " + (chain == 0 ? "transitive" : "control")
                + " built=" + built + " con=" + CON[chain].get() + " uncon=" + UNCON[chain].get());
    }

    public static void main(String[] args) {
        long wt = 0, wc = 0;
        for (int r = 0; r < 4; r++) {
            wt += warmT(WARM / 4);
            wc += warmC(WARM / 4);
        }
        // One long call each so the loop is OSR-compiled too.
        wt += warmT(200_000);
        wc += warmC(200_000);
        System.out.println("warm t=" + wt + " c=" + wc);

        int builtT = leafT(N);
        report("leaf", 0, builtT, builtT);
        int builtC = leafC(N);
        report("leaf", 1, builtC, builtC);

        // Hot leaf constructors, then the same counts again (cumulative).
        for (int r = 0; r < 10; r++) {
            builtT += leafT(N);
            builtC += leafC(N);
        }
        report("leaf2", 0, builtT, builtT);
        report("leaf2", 1, builtC, builtC);
        System.out.println("done");
    }
}

class R21Fz5TBase {
    int v;

    R21Fz5TBase(String s) {
        this(Integer.parseInt(s));
    }

    R21Fz5TBase(int v) {
        this.v = v;
    }
}

@SuppressWarnings("removal")
class R21Fz5TFin extends R21Fz5TBase {
    boolean ok;

    R21Fz5TFin(String s) {
        super(s);
    }

    @Override
    protected void finalize() {
        R21Fz5FinTransitive1.fin(ok, 0);
    }
}

class R21Fz5TLeaf extends R21Fz5TFin {
    R21Fz5TLeaf(String s) {
        super(s);
        ok = true;
    }
}

class R21Fz5CBase {
    int v;

    R21Fz5CBase(String s) {
        this(Integer.parseInt(s));
    }

    R21Fz5CBase(int v) {
        this.v = v;
    }
}

@SuppressWarnings("removal")
class R21Fz5CFin extends R21Fz5CBase {
    boolean ok;

    R21Fz5CFin(String s) {
        super(s);
        ok = true;
    }

    @Override
    protected void finalize() {
        R21Fz5FinTransitive1.fin(ok, 1);
    }
}
