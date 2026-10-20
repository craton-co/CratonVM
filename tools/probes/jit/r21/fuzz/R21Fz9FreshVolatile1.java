// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 23, lane fuzz9: volatile stores into freshly allocated
// objects, in constructors and factories, where the object escapes at
// different points, read by a second thread that synchronizes ONLY through
// the volatile publication (or through the volatile field itself), never
// through a separate sequence counter.
//
// Shapes (each N = 20000 rounds, writer and reader in lockstep: the writer
// publishes object i, the reader finds it, checks it, acknowledges i):
//   S0 ctor stores volatile int v, volatile Object r, plain int p; the CALLER
//      publishes to a static volatile.
//   S1 ctor stores, then publishes `this` to the static volatile (escape at the
//      END of the constructor).
//   S2 ctor publishes `this` FIRST, then stores plain p, then the volatile flag
//      v LAST; the reader spins on v != 0, then must see p (volatile v gives
//      happens-before for p).
//   S3 factory: allocate, escape to a PLAIN static, volatile stores, publish a
//      capturing lambda (Supplier) through a static volatile.
//   S4 factory: allocate an inner object, volatile-store it into a freshly
//      allocated outer object, publish the outer through an AtomicReference.
//   S5 factory: the object travels inside a lambda to a single-thread executor
//      (BlockingQueue hand-off), which checks it; results via Future.get.
//   S6 subclass constructor: the superclass ctor publishes `this` after ITS
//      volatile stores; the subclass then stores its own volatile w and plain q
//      (reader checks the super's fields immediately, spins on w, then q).
// "bad" counts a default value (0 / null) or a wrong value seen where the
// publication guarantees the stored one; "stuck" counts spins that hit the
// bound (the writer's volatile store never became visible).
// The writer also re-reads its own fresh object right after construction
// ("self" bad count: store-to-load forwarding across the elided fence).
//
// Risk targeted: wave 22's opt-in "fresh-allocation volatile store unfenced"
// (a volatile store into an object that has not escaped needs no fence): an
// escape analysis that misses `this` published from a constructor, a capture
// in a lambda, a store into another fresh object, or a superclass escape.
//
// Run:
//   javac -d out R21Fz9FreshVolatile1.java
//   java -Xmx256m -cp out R21Fz9FreshVolatile1            # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz9FreshVolatile1        # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz9FreshVolatile1
//   cratonvm --nojit -Xmx256m -cp out R21Fz9FreshVolatile1
//   (and with the wave 22 fresh-allocation volatile store opt-in switched on)
//
// HotSpot 25 expectation (JLS 17.4 happens-before; not yet run):
//   S0 rounds=20000 bad=0 stuck=0 self=0
//   S1 rounds=20000 bad=0 stuck=0 self=0
//   S2 rounds=20000 bad=0 stuck=0 self=0
//   S3 rounds=20000 bad=0 stuck=0 self=0
//   S4 rounds=20000 bad=0 stuck=0 self=0
//   S5 rounds=20000 bad=0 stuck=0 self=0
//   S6 rounds=20000 bad=0 stuck=0 self=0
//   reader done
// On x86 a missing hardware fence on a plain store is invisible (TSO); what
// this probe can catch there is a compiler reordering / store sinking or an
// elided store, e.g. the flag store moved before p, or v never written.

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Supplier;

public class R21Fz9FreshVolatile1 {
    static final int N = 20_000;
    static final long SPIN_BOUND = 400_000_000L;
    static final Object TAG = new Object();

    static volatile Object PUB;
    static volatile Supplier<Box> PUBS;
    static final AtomicReference<Outer> PUBA = new AtomicReference<>();
    static Box PLAIN_ESCAPE;

    static volatile int ACK = -1;
    static volatile int SHAPE = -1;
    static volatile boolean QUIT;

    static int val(int i) {
        return 2 * i + 1;
    }

    static final class Box {
        volatile int v;
        volatile Object r;
        int p;

        Box() {
        }

        Box(int x) {
            v = x;
            r = TAG;
            p = x;
        }

        Box(int x, boolean publishAtEnd) {
            v = x;
            r = TAG;
            p = x;
            if (publishAtEnd) {
                PUB = this;
            }
        }

        Box(int x, String publishFirst) {
            PUB = this;
            p = x;
            r = TAG;
            v = x;
        }
    }

    static final class Inner {
        volatile int v;
        int p;

        Inner(int x) {
            p = x;
            v = x;
        }
    }

    static final class Outer {
        volatile Inner in;
        volatile int tag;
    }

    static class Sup {
        volatile int sv;
        volatile Object sr;

        Sup(int x) {
            sv = x;
            sr = TAG;
            PUB = this;
        }
    }

    static final class SubPub extends Sup {
        volatile int w;
        int q;

        SubPub(int x) {
            super(x);
            q = x;
            w = x;
        }
    }

    static int selfBad;

    // ---- writer-side builders (hot: 20000 calls each) ----

    static Box s0(int x) {
        Box b = new Box(x);
        if (b.v != x || b.r != TAG || b.p != x) {
            selfBad++;
        }
        PUB = b;
        return b;
    }

    static Box s1(int x) {
        Box b = new Box(x, true);
        if (b.v != x || b.r != TAG || b.p != x) {
            selfBad++;
        }
        return b;
    }

    static Box s2(int x) {
        Box b = new Box(x, "first");
        if (b.v != x || b.r != TAG || b.p != x) {
            selfBad++;
        }
        return b;
    }

    static Box s3(int x) {
        Box b = new Box();
        PLAIN_ESCAPE = b;
        b.p = x;
        b.r = TAG;
        b.v = x;
        if (b.v != x || b.r != TAG || PLAIN_ESCAPE.p != x) {
            selfBad++;
        }
        PUBS = () -> b;
        return b;
    }

    static Outer s4(int x) {
        Outer o = new Outer();
        o.in = new Inner(x);
        o.tag = x;
        if (o.in.v != x || o.in.p != x || o.tag != x) {
            selfBad++;
        }
        PUBA.set(o);
        return o;
    }

    static Box s5(int x) {
        Box b = new Box();
        b.p = x;
        b.v = x;
        b.r = TAG;
        if (b.v != x || b.p != x) {
            selfBad++;
        }
        return b;
    }

    static SubPub s6(int x) {
        SubPub s = new SubPub(x);
        if (s.sv != x || s.sr != TAG || s.w != x || s.q != x) {
            selfBad++;
        }
        return s;
    }

    // ---- reader ----

    static int bad;
    static int stuck;

    static boolean spinUntil(java.util.function.BooleanSupplier c) {
        long n = 0;
        while (!c.getAsBoolean()) {
            if (++n > SPIN_BOUND || QUIT) {
                return false;
            }
            if ((n & 1023) == 0) {
                Thread.yield();
            } else {
                Thread.onSpinWait();
            }
        }
        return true;
    }

    static Object lastSeen;

    // Waits for a NEW published object of the current shape; checks it.
    static void readOne(int shape, int i) {
        int x = val(i);
        switch (shape) {
            case 0, 1 -> {
                if (!spinUntil(() -> PUB != lastSeen && PUB != null)) {
                    stuck++;
                    return;
                }
                Box b = (Box) PUB;
                lastSeen = b;
                if (b.v != x || b.r != TAG || b.p != x) {
                    bad++;
                }
            }
            case 2 -> {
                if (!spinUntil(() -> PUB != lastSeen && PUB != null)) {
                    stuck++;
                    return;
                }
                Box b = (Box) PUB;
                lastSeen = b;
                if (!spinUntil(() -> b.v != 0)) {
                    stuck++;
                    return;
                }
                if (b.v != x || b.p != x || b.r != TAG) {
                    bad++;
                }
            }
            case 3 -> {
                if (!spinUntil(() -> PUBS != lastSeen && PUBS != null)) {
                    stuck++;
                    return;
                }
                Supplier<Box> s = PUBS;
                lastSeen = s;
                Box b = s.get();
                if (b.v != x || b.r != TAG || b.p != x) {
                    bad++;
                }
            }
            case 4 -> {
                if (!spinUntil(() -> PUBA.get() != lastSeen && PUBA.get() != null)) {
                    stuck++;
                    return;
                }
                Outer o = PUBA.get();
                lastSeen = o;
                Inner in = o.in;
                if (in == null || in.v != x || in.p != x || o.tag != x) {
                    bad++;
                }
            }
            case 6 -> {
                if (!spinUntil(() -> PUB != lastSeen && PUB != null)) {
                    stuck++;
                    return;
                }
                SubPub s = (SubPub) PUB;
                lastSeen = s;
                if (s.sv != x || s.sr != TAG) {
                    bad++;
                }
                if (!spinUntil(() -> s.w != 0)) {
                    stuck++;
                    return;
                }
                if (s.w != x || s.q != x) {
                    bad++;
                }
            }
            default -> throw new AssertionError(shape);
        }
    }

    static void reader() {
        int shape = -1;
        int i = 0;
        while (!QUIT) {
            int sh = SHAPE;
            if (sh != shape) {
                shape = sh;
                i = 0;
                if (shape < 0) {
                    Thread.onSpinWait();
                    continue;
                }
            }
            if (shape < 0 || i >= N) {
                Thread.onSpinWait();
                continue;
            }
            readOne(shape, i);
            ACK = shape * N + i;
            i++;
        }
    }

    static void waitAck(int want) {
        if (!spinUntil(() -> ACK == want)) {
            System.out.println("writer: reader lost at " + want);
            QUIT = true;
            System.exit(1);
        }
    }

    static void runShape(int shape) {
        PUB = null;
        PUBS = null;
        PUBA.set(null);
        selfBad = 0;
        int bad0 = bad;
        int stuck0 = stuck;
        int ackBase = shape * N;
        SHAPE = shape;
        long sink = 0;
        for (int i = 0; i < N; i++) {
            int x = val(i);
            switch (shape) {
                case 0 -> sink += s0(x).p;
                case 1 -> sink += s1(x).p;
                case 2 -> sink += s2(x).p;
                case 3 -> sink += s3(x).p;
                case 4 -> sink += s4(x).tag;
                case 6 -> sink += s6(x).q;
                default -> throw new AssertionError();
            }
            waitAck(ackBase + i);
        }
        System.out.println("S" + shape + " rounds=" + N + " bad=" + (bad - bad0) + " stuck="
                + (stuck - stuck0) + " self=" + selfBad + (sink == (long) N * N ? "" : " sink?"));
    }

    static int check5(Box b, int x) {
        return (b.v != x || b.p != x || b.r != TAG) ? 1 : 0;
    }

    static void runS5() throws Exception {
        selfBad = 0;
        ExecutorService ex = Executors.newSingleThreadExecutor();
        int badHere = 0;
        long sink = 0;
        try {
            for (int i = 0; i < N; i++) {
                int x = val(i);
                Box b = s5(x);
                sink += b.p;
                Future<Integer> f = ex.submit(() -> check5(b, x));
                badHere += f.get();
            }
        } finally {
            ex.shutdown();
        }
        System.out.println("S5 rounds=" + N + " bad=" + badHere + " stuck=0 self=" + selfBad
                + (sink == (long) N * N ? "" : " sink?"));
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(R21Fz9FreshVolatile1::reader, "fz9-reader");
        t.setDaemon(true);
        t.start();
        runShape(0);
        runShape(1);
        runShape(2);
        runShape(3);
        runShape(4);
        runS5();
        runShape(6);
        QUIT = true;
        t.join(10_000);
        System.out.println(t.isAlive() ? "reader alive" : "reader done");
    }
}
