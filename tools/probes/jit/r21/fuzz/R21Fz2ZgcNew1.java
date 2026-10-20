// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: ALLOC21-2, ZGC's post-init `new` called
// helper-first (`CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST`, default ON;
// `InlineTlabPlan::post_init_helper_first`, set by `ir_lower`'s `Op::New` arm
// only for a class in `Graph::finalizer_free_new_classes`).
//
// Risk targeted: the helper now runs with the TLAB cursor UNCOMMITTED. That
// is sound only if the helper allocates nothing from this TLAB, which is why
// a finalizable class (registration may allocate) and a java.lang.ref class
// (discovery registration) must keep commit-then-call. A class wrongly
// proved finalizer-free would either lose its registration (finalize()
// never runs; a WeakReference never clears / enqueues) or have the helper's
// allocation overlap the object. Rows: a plain class with every field type
// (defaults must read 0 / null / false before the constructor writes), a
// finalizable class and a SUBCLASS of one (finalizer inherited), a class
// whose superclass is finalizer-free but which itself declares finalize(),
// WeakReference / SoftReference / PhantomReference and a WeakReference
// subclass created hot with a queue, a constructor that throws after the
// object exists. Counts are exact: the probe retries System.gc() until the
// expected number is reached (bounded), and prints only whether it was.
// A second thread calls System.gc() in a loop while the main thread builds a
// linked list from compiled `new`s; the list's checksum is exact.
//
// Run:
//   javac -d out R21Fz2ZgcNew1.java
//   java -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew1                       # HotSpot 25
//   cratonvm -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew1
//   CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST=0 cratonvm -Xmx512m -XX:+UseZGC -cp out R21Fz2ZgcNew1 (kill)
//   CRATONVM_ZGC_JIT_HELPER_BEFORE_COMMIT=0 cratonvm -Xmx512m -XX:+UseZGC ...                   (control)
//   cratonvm -Xmx512m -cp out R21Fz2ZgcNew1, ... -XX:+UseG1GC ...           (other collectors: no change)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm. Finalization is
// deprecated but enabled by default in JDK 25 (do not pass --finalization=disabled).

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.concurrent.atomic.AtomicInteger;

public class R21Fz2ZgcNew1 {
    static final int WARM = 200_000;
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final AtomicInteger SUB_FINALIZED = new AtomicInteger();
    static final AtomicInteger OWN_FINALIZED = new AtomicInteger();
    static volatile boolean stop;

    static final class Plain {
        byte b;
        short s;
        char c;
        int i;
        long l;
        float f;
        double d;
        boolean z;
        Object o;
        String seen;

        Plain(int v) {
            // The defaults, read before any write: all zero / null / false.
            seen = "" + b + s + (int) c + i + l + f + d + z + o;
            i = v;
            l = v * 3L;
            o = this;
        }
    }

    static class Fin {
        final int v;

        Fin(int v) {
            this.v = v;
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static final class FinSub extends Fin {
        FinSub(int v) {
            super(v);
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            SUB_FINALIZED.incrementAndGet();
        }
    }

    static class Base {
        int k;
    }

    static final class OwnFin extends Base {
        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            OWN_FINALIZED.incrementAndGet();
        }
    }

    static final class MyWeak extends WeakReference<Object> {
        final int tag;

        MyWeak(Object referent, ReferenceQueue<Object> q, int tag) {
            super(referent, q);
            this.tag = tag;
        }
    }

    static final class Thrower {
        int v;

        Thrower(int v) {
            this.v = v;
            if (v % 1000 == 999) {
                throw new IllegalStateException("t" + v);
            }
        }
    }

    static final class Node {
        final int v;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static int plain(int v) {
        Plain p = new Plain(v);
        return p.seen.length() + p.i + (p.o == p ? 1 : 0);
    }

    static int fin(int v) {
        return new Fin(v).v + new FinSub(v).v + (new OwnFin().k);
    }

    static boolean waitFor(AtomicInteger counter, int want) {
        for (int i = 0; i < 400 && counter.get() < want; i++) {
            System.gc();
            System.runFinalization();
            try {
                Thread.sleep(2);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        return counter.get() == want;
    }

    static int drain(ReferenceQueue<Object> q, int want) {
        int got = 0;
        for (int i = 0; i < 400 && got < want; i++) {
            System.gc();
            Reference<?> r;
            while ((r = q.poll()) != null) {
                got++;
            }
            if (got < want) {
                try {
                    Thread.sleep(2);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }
        return got;
    }

    public static void main(String[] args) throws Exception {
        System.out.println("row cold plain=" + plain(7) + " seen=" + new Plain(1).seen + " fin=" + fin(3));
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + plain(r);
        }
        System.out.println("warm plain hash " + h);
        int fins = 20_000;
        for (int r = 0; r < fins; r++) {
            h = h * 31 + fin(r);
        }
        System.out.println("warm fin hash " + h);
        System.out.println("finalized Fin " + waitFor(FINALIZED, fins + 1) + " FinSub " + waitFor(SUB_FINALIZED, fins + 1)
                + " OwnFin " + waitFor(OWN_FINALIZED, fins + 1));

        // References created hot, referents dropped at once.
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        // The soft references' referents stay strongly reachable (slot 4), so
        // none of them may clear or enqueue; every weak / phantom one must.
        Object[] keep = new Object[5 * 50_000];
        int kept = 0;
        for (int r = 0; r < 50_000; r++) {
            Object strong = new Plain(r);
            keep[kept++] = new WeakReference<>(new Plain(r), q);
            keep[kept++] = new MyWeak(new Plain(-r), q, r);
            keep[kept++] = new PhantomReference<>(new Plain(r + 1), q);
            keep[kept++] = new SoftReference<>(strong, q);
            keep[kept++] = strong;
        }
        int got = drain(q, 3 * 50_000);
        int weakLive = 0;
        int softLive = 0;
        for (int i = 0; i < kept; i += 5) {
            weakLive += ((WeakReference<?>) keep[i]).get() == null ? 0 : 1;
            weakLive += ((MyWeak) keep[i + 1]).get() == null ? 0 : 1;
            softLive += ((SoftReference<?>) keep[i + 3]).get() == keep[i + 4] ? 1 : 0;
        }
        System.out.println("refs enqueued " + got + " weakLive " + weakLive + " softLive " + softLive + " tag "
                + ((MyWeak) keep[kept - 4]).tag);

        // A constructor that throws after the object exists.
        int threw = 0;
        long tv = 0;
        for (int r = 0; r < 100_000; r++) {
            try {
                tv += new Thrower(r).v;
            } catch (IllegalStateException e) {
                threw++;
            }
        }
        System.out.println("thrower " + threw + " " + tv);

        // Concurrent collections while compiled `new`s build a list.
        Thread gc = new Thread(() -> {
            while (!stop) {
                System.gc();
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
        });
        gc.start();
        long sum = 0;
        for (int round = 0; round < 20; round++) {
            Node head = null;
            for (int i = 0; i < 20_000; i++) {
                head = new Node(i ^ round, head);
                if ((i & 4095) == 0) {
                    sum += plain(i);
                }
            }
            for (Node n = head; n != null; n = n.next) {
                sum = sum * 31 + n.v;
            }
        }
        stop = true;
        gc.join();
        System.out.println("list checksum " + sum);
        System.out.println("row after plain=" + plain(7) + " seen=" + new Plain(1).seen + " fin=" + fin(3));
    }
}
