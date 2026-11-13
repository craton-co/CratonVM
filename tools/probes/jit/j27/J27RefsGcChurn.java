// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane REFS): pages
// docs/feature-designs/generative-gc-architecture.md
// docs/feature-designs/generative-gc-architecture.md
//
// Compiled Reference.get() / refersTo() under GC churn: a held referent stays
// readable through collections, a referent only a compiled refersTo() looked at
// is still cleared and enqueued (no keep-alive), a referent a compiled get()
// handed out stays alive, clear() reads null at once, and WeakHashMap expunges.
// Run under each collector (G1, ZGC, Generational); the facts are the same.
// Facts are deterministic; timing lines only with -Dj27.time=true.
import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.WeakHashMap;

public class J27RefsGcChurn {
    static final int N = 256;
    static final Object SENTINEL = new Object();
    static volatile Object[] garbageSink;

    static final class Sub extends WeakReference<Object> {
        final int tag;
        Sub(Object o, int tag) { super(o); this.tag = tag; }
    }

    // The kernels the JIT compiles (warmed in main before any fact is read).
    static boolean getIs(Reference<Object> r, Object o) { return r.get() == o; }
    static boolean weakGetIs(WeakReference<Object> r, Object o) { return r.get() == o; }
    static boolean softGetIs(SoftReference<Object> r, Object o) { return r.get() == o; }
    static boolean refersTo(Reference<Object> r, Object o) { return r.refersTo(o); }
    static boolean weakRefersTo(WeakReference<Object> r, Object o) { return r.refersTo(o); }
    static boolean phantomRefersTo(PhantomReference<Object> r, Object o) { return r.refersTo(o); }
    static Object weakGet(WeakReference<Object> r) { return r.get(); }

    static void churn(int rounds) {
        for (int i = 0; i < rounds; i++) {
            Object[] g = new Object[512];
            for (int j = 0; j < g.length; j++) g[j] = new byte[64 + (j & 63)];
            garbageSink = g;
        }
        garbageSink = null;
    }

    static boolean clearedByGc(WeakReference<Object> r) {
        for (int i = 0; i < 50 && !weakRefersTo(r, null); i++) {
            churn(4);
            System.gc();
        }
        return weakRefersTo(r, null);
    }

    static Object comparedHolder;

    // The referent is held (a static) while the compiled kernels compare it,
    // then dropped: from there on only compiled refersTo() reads it (the
    // clear loop's refersTo(null)), which must not keep it alive.
    static WeakReference<Object> onlyComparedWeak(ReferenceQueue<Object> q) {
        comparedHolder = new Object();
        WeakReference<Object> r = new WeakReference<>(comparedHolder, q);
        int hits = 0;
        for (int i = 0; i < 100_000; i++) {
            if (weakRefersTo(r, SENTINEL)) hits++;
            if (refersTo(r, null)) hits++;
            if (!refersTo(r, comparedHolder)) hits++;
        }
        comparedHolder = null;
        return hits == 0 ? r : null;
    }

    public static void main(String[] args) throws Exception {
        boolean time = Boolean.getBoolean("j27.time");
        Object[] targets = new Object[N];
        @SuppressWarnings("unchecked")
        WeakReference<Object>[] weak = new WeakReference[N];
        @SuppressWarnings("unchecked")
        SoftReference<Object>[] soft = new SoftReference[N];
        Sub[] sub = new Sub[N];
        @SuppressWarnings("unchecked")
        PhantomReference<Object>[] phantom = new PhantomReference[N];
        ReferenceQueue<Object> pq = new ReferenceQueue<>();
        for (int i = 0; i < N; i++) {
            targets[i] = new int[] {i};
            weak[i] = new WeakReference<>(targets[i]);
            soft[i] = new SoftReference<>(targets[i]);
            sub[i] = new Sub(targets[i], i);
            phantom[i] = new PhantomReference<>(targets[i], pq);
        }

        // Warm-up: every kernel compiled on compact receivers.
        long sink = 0;
        for (int w = 0; w < 400; w++) {
            for (int i = 0; i < N; i++) {
                Object t = targets[i];
                if (weakGetIs(weak[i], t)) sink++;
                if (softGetIs(soft[i], t)) sink++;
                if (getIs(sub[i], t)) sink++;
                if (refersTo(weak[i], t)) sink++;
                if (weakRefersTo(sub[i], t)) sink++;
                if (phantomRefersTo(phantom[i], t)) sink++;
            }
        }

        // Churn: held referents, read by the compiled kernels between GCs.
        long held = 0;
        for (int round = 0; round < 40; round++) {
            churn(8);
            if ((round & 7) == 0) System.gc();
            for (int i = 0; i < N; i++) {
                Object t = targets[i];
                if (weakGetIs(weak[i], t) && softGetIs(soft[i], t) && getIs(sub[i], t)
                        && refersTo(weak[i], t) && weakRefersTo(sub[i], t)
                        && phantomRefersTo(phantom[i], t) && !refersTo(weak[i], null)
                        && phantom[i].get() == null) {
                    held++;
                }
            }
        }
        System.out.println("fact churn-held " + held);

        // A referent only compared by compiled refersTo() is still cleared and
        // enqueued: no keep-alive (HotSpot's AS_NO_KEEPALIVE refersTo0 load).
        ReferenceQueue<Object> wq = new ReferenceQueue<>();
        WeakReference<Object> compared = onlyComparedWeak(wq);
        System.out.println("fact compared-only-live " + (compared != null));
        boolean cleared = compared != null && clearedByGc(compared);
        System.out.println("fact refersto-no-keepalive " + cleared);
        Reference<?> polled = cleared ? wq.remove(10_000) : null;
        System.out.println("fact compared-enqueued " + (polled == compared));
        System.out.println("fact compared-get-null " + (compared != null && compared.get() == null));

        // A referent a compiled get() handed out stays alive while held.
        WeakReference<Object> handed = new WeakReference<>(new int[] {42});
        Object kept = null;
        for (int i = 0; i < 100_000; i++) kept = weakGet(handed);
        int idBefore = System.identityHashCode(kept);
        for (int i = 0; i < 6; i++) { churn(4); System.gc(); }
        System.out.println("fact get-kept-alive " + (weakRefersTo(handed, kept)
                && System.identityHashCode(kept) == idBefore && ((int[]) kept)[0] == 42));
        kept = null;
        System.out.println("fact get-dropped-cleared " + clearedByGc(handed));

        // clear() reads null at once through the compiled kernels.
        WeakReference<Object> cl = new WeakReference<>(targets[0]);
        cl.clear();
        System.out.println("fact clear-refersto-null " + refersTo(cl, null));
        System.out.println("fact clear-get-null " + weakGetIs(cl, null));
        System.out.println("fact clear-refersto-target " + weakRefersTo(cl, targets[0]));

        // A phantom referent dropped: cleared and enqueued.
        Object pt = targets[1];
        PhantomReference<Object> p1 = phantom[1];
        targets[1] = null;
        weak[1] = null;
        soft[1] = null;
        sub[1] = null;
        pt = null;
        Reference<?> pp = null;
        for (int i = 0; i < 50 && pp == null; i++) {
            churn(4);
            System.gc();
            pp = pq.remove(50);
        }
        System.out.println("fact phantom-enqueued " + (pp == p1));
        System.out.println("fact phantom-refersto-null " + phantomRefersTo(p1, null));

        // WeakHashMap: held keys stay, dropped keys expunge.
        WeakHashMap<Object, Integer> whm = new WeakHashMap<>();
        Object[] keys = new Object[1000];
        for (int i = 0; i < keys.length; i++) {
            keys[i] = new Object();
            whm.put(keys[i], i);
        }
        for (int i = 0; i < keys.length; i += 2) keys[i] = null;
        for (int i = 0; i < 50 && whm.size() != 500; i++) {
            churn(4);
            System.gc();
        }
        int found = 0;
        for (int i = 1; i < keys.length; i += 2) if (whm.get(keys[i]) == i) found++;
        System.out.println("fact whm-size " + whm.size());
        System.out.println("fact whm-found " + found);
        System.out.println("fact sub-tag " + sub[2].tag);
        System.out.println("fact sink-positive " + (sink > 0));

        if (time) {
            String[] rows = {"weak-get", "weak-refersto", "phantom-refersto", "sub-get"};
            for (String row : rows) {
                long t0 = System.nanoTime();
                int reps = 200_000;
                for (int r = 0; r < reps; r++) {
                    Object t = targets[2];
                    for (int k = 0; k < 16; k++) {
                        boolean b = switch (row) {
                            case "weak-get" -> weakGetIs(weak[2], t);
                            case "weak-refersto" -> weakRefersTo(weak[2], t);
                            case "phantom-refersto" -> phantomRefersTo(phantom[2], t);
                            default -> getIs(sub[2], t);
                        };
                        if (b) sink++;
                    }
                }
                long ns = (System.nanoTime() - t0) / reps;
                System.out.println("time " + row + " " + ns + " ns/16");
            }
        }
        if (sink == 42) System.out.println("unlikely");
        Reference.reachabilityFence(targets);
        Reference.reachabilityFence(keys);
    }
}
