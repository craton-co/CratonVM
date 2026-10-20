// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Cleaner;
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.WeakHashMap;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * SBR round 3, lane {@code ref}: the {@code java.lang.ref} constructors, {@code get} and
 * {@code refersTo} as the JDK's own bytecode, with the reference processor learning of a
 * reference at its allocation. One line per row; diff against HotSpot (output is deterministic).
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefRefs [hotIterations]
 * </pre>
 */
public final class Sbr3RefRefs {
    static final class Tagged extends WeakReference<Object> {
        final int tag;

        Tagged(Object referent, ReferenceQueue<Object> q, int tag) {
            super(referent, q);
            this.tag = tag;
        }
    }

    static final class TaggedSoft extends SoftReference<Object> {
        final String name;

        TaggedSoft(Object referent, ReferenceQueue<Object> q, String name) {
            super(referent, q);
            this.name = name;
        }
    }

    static final class TaggedPhantom extends PhantomReference<Object> {
        final int n;

        TaggedPhantom(Object referent, ReferenceQueue<Object> q, int n) {
            super(referent, q);
            this.n = n;
        }
    }

    static void gcUntil(java.util.function.BooleanSupplier done) throws InterruptedException {
        for (int i = 0; i < 40 && !done.getAsBoolean(); i++) {
            System.gc();
            if (!done.getAsBoolean()) {
                Thread.sleep(15);
            }
        }
    }

    static WeakReference<Object> weak(ReferenceQueue<Object> q) {
        return q == null ? new WeakReference<>(new Object()) : new WeakReference<>(new Object(), q);
    }

    static SoftReference<Object> soft(ReferenceQueue<Object> q) {
        return q == null ? new SoftReference<>(new byte[32]) : new SoftReference<>(new byte[32], q);
    }

    static PhantomReference<Object> phantom(ReferenceQueue<Object> q) {
        return new PhantomReference<>(new int[8], q);
    }

    static Tagged tagged(ReferenceQueue<Object> q, int tag) {
        return new Tagged(new long[2], q, tag);
    }

    static String show(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    public static void main(String[] args) throws Exception {
        int hot = args.length > 0 ? Integer.parseInt(args[0]) : 30000;

        // ---- constructors, get, refersTo while the referent is alive
        Object strong = new Object();
        WeakReference<Object> w1 = new WeakReference<>(strong);
        System.out.println("weak get same=" + (w1.get() == strong) + " refersTo=" + w1.refersTo(strong)
                + " refersToNull=" + w1.refersTo(null) + " enqueue=" + w1.enqueue() + " isEnq=" + w1.isEnqueued());
        SoftReference<Object> s1 = new SoftReference<>(strong);
        System.out.println("soft get same=" + (s1.get() == strong) + " refersTo=" + s1.refersTo(strong)
                + " refersToNull=" + s1.refersTo(null));
        ReferenceQueue<Object> q0 = new ReferenceQueue<>();
        PhantomReference<Object> p1 = new PhantomReference<>(strong, q0);
        System.out.println("phantom get=" + p1.get() + " refersTo=" + p1.refersTo(strong)
                + " refersToNull=" + p1.refersTo(null));
        WeakReference<Object> wNull = new WeakReference<>(null);
        System.out.println("weak null get=" + wNull.get() + " refersToNull=" + wNull.refersTo(null));
        WeakReference<Object> wq = new WeakReference<>(strong, q0);
        System.out.println("weak+queue enqueue=" + wq.enqueue() + " again=" + wq.enqueue() + " poll same="
                + (q0.poll() == wq) + " pollAgain=" + q0.poll());
        WeakReference<Object> wc = new WeakReference<>(strong);
        wc.clear();
        System.out.println("clear get=" + wc.get() + " refersToNull=" + wc.refersTo(null) + " strongAlive=" + (strong != null));
        try {
            new PhantomReference<Object>(strong, null).enqueue();
            System.out.println("phantom null-queue enqueue ok");
        } catch (Throwable t) {
            System.out.println("phantom null-queue enqueue threw " + t);
        }
        SoftReference<Object> sTimed = new SoftReference<>(strong);
        System.out.println("soft touched again=" + (sTimed.get() == strong));

        // ---- collector: weak, queue, subclass, phantom, soft-recent
        ReferenceQueue<Object> wqueue = new ReferenceQueue<>();
        WeakReference<Object> weakDead = weak(wqueue);
        WeakReference<Object> weakNoQ = weak(null);
        ReferenceQueue<Object> pqueue = new ReferenceQueue<>();
        PhantomReference<Object> phantomDead = phantom(pqueue);
        ReferenceQueue<Object> tqueue = new ReferenceQueue<>();
        Tagged tg = tagged(tqueue, 7);
        ReferenceQueue<Object> squeue = new ReferenceQueue<>();
        TaggedSoft ts = new TaggedSoft(new byte[64], squeue, "soft-sub");
        TaggedPhantom tp = new TaggedPhantom(new Object(), pqueue, 3);
        SoftReference<Object> softRecent = soft(null);
        gcUntil(() -> weakDead.refersTo(null) && tg.refersTo(null) && weakNoQ.refersTo(null));
        System.out.println("weakDead cleared=" + weakDead.refersTo(null) + " getNull=" + (weakDead.get() == null));
        System.out.println("weakNoQ cleared=" + weakNoQ.refersTo(null));
        Reference<?> got = wqueue.remove(2000);
        System.out.println("weakDead enqueued=" + (got == weakDead) + " isEnq=" + weakDead.isEnqueued());
        Reference<?> tgot = tqueue.remove(2000);
        System.out.println("tagged cleared=" + tg.refersTo(null) + " enqueued=" + (tgot == tg) + " tag=" + ((Tagged) tgot).tag);
        int phantoms = 0;
        boolean sawP1 = false, sawTp = false;
        for (int i = 0; i < 2; i++) {
            Reference<?> r = pqueue.remove(2000);
            if (r != null) {
                phantoms++;
                sawP1 |= r == phantomDead;
                sawTp |= r == tp;
            }
        }
        System.out.println("phantoms enqueued=" + phantoms + " plain=" + sawP1 + " sub=" + sawTp
                + " refersToNull(sub)=" + tp.refersTo(null) + " get=" + tp.get());
        System.out.println("soft sub kept=" + (ts.get() != null) + " name=" + ts.name + " queueEmpty=" + (squeue.poll() == null));
        System.out.println("soft recent kept=" + (softRecent.get() != null));

        // ---- reference to a reference, and a self-referential holder
        WeakReference<Object> inner = new WeakReference<>(new Object());
        WeakReference<Object> outer = new WeakReference<>(inner);
        gcUntil(() -> inner.refersTo(null));
        System.out.println("outer still holds inner=" + (outer.get() == inner || outer.get() == null) + " innerCleared=" + inner.refersTo(null));

        // ---- WeakHashMap expunge, ThreadLocal remove, java.lang.ref.Cleaner action
        WeakHashMap<Object, String> whm = new WeakHashMap<>();
        for (int i = 0; i < 50; i++) {
            whm.put(new Object(), "v" + i);
        }
        gcUntil(() -> whm.size() == 0);
        System.out.println("WeakHashMap size after gc=" + whm.size());
        ThreadLocal<Object> tl = new ThreadLocal<>();
        tl.set("x");
        tl.remove();
        tl.set("y");
        System.out.println("ThreadLocal get=" + tl.get());
        Cleaner cleaner = Cleaner.create();
        AtomicInteger ran = new AtomicInteger();
        registerCleanable(cleaner, ran);
        gcUntil(() -> ran.get() == 1);
        System.out.println("Cleaner action ran=" + ran.get());
        Cleaner.Cleanable explicit = cleaner.register(new Object(), ran::incrementAndGet);
        explicit.clean();
        explicit.clean();
        System.out.println("Cleaner explicit ran=" + ran.get());

        // ---- jdk.internal.ref.Cleaner path: direct buffers reclaimed
        long total = 0;
        for (int i = 0; i < 400; i++) {
            ByteBuffer bb = ByteBuffer.allocateDirect(64 * 1024);
            total += bb.capacity();
        }
        System.out.println("direct buffers allocated bytes=" + total);

        // ---- hot loop: compiled `new` of every kind registers, and a stress count is exact
        ReferenceQueue<Object> hq = new ReferenceQueue<>();
        List<Reference<Object>> keep = new ArrayList<>();
        int made = 0;
        for (int i = 0; i < hot; i++) {
            Reference<Object> r;
            switch (i % 3) {
                case 0:
                    r = new WeakReference<>(new Object(), hq);
                    break;
                case 1:
                    r = new Tagged(new Object(), hq, i);
                    break;
                default:
                    r = new WeakReference<>(new Object());
                    break;
            }
            if (i % 997 == 0) {
                keep.add(r);
            }
            made++;
        }
        gcUntil(() -> {
            for (Reference<Object> r : keep) {
                if (!r.refersTo(null)) {
                    return false;
                }
            }
            return true;
        });
        int clearedKept = 0;
        for (Reference<Object> r : keep) {
            if (r.refersTo(null)) {
                clearedKept++;
            }
        }
        System.out.println("hot made=" + made + " kept=" + keep.size() + " clearedKept=" + clearedKept);
        int drained = 0;
        while (hq.remove(300) != null) {
            drained++;
            if (drained > hot) {
                break;
            }
        }
        // the collector cleared and enqueued queued references (HotSpot discovers only the live ones, so
        // the count is collector-dependent; the shape is not)
        System.out.println("hot enqueued=" + (drained > 0));
        Map<String, Object> m = new java.util.HashMap<>();
        m.put("done", Boolean.TRUE);
        System.out.println("PROBE-DONE " + m);
    }

    static void registerCleanable(Cleaner c, AtomicInteger ran) {
        c.register(new Object(), ran::incrementAndGet);
    }
}
