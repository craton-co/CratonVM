// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.WeakHashMap;

/**
 * SBR round 4, lane {@code satb}: deterministic Java-visible behaviour of {@code Reference.get}
 * (bytecode, retired rows), diffed against HotSpot. Every row prints one line; nothing depends
 * on collector timing except after an explicit {@code System.gc()} on an unreachable referent
 * (HotSpot clears it; both modes must).
 */
public final class Sbr4SatbGet {
    static final class Box {
        final int v;
        Box(int v) { this.v = v; }
        public String toString() { return "Box" + v; }
    }

    static final class MyWeak<T> extends WeakReference<T> {
        int gets;
        MyWeak(T t) { super(t); }
        @Override public T get() { gets++; return super.get(); }
    }

    static void line(String k, Object v) { System.out.println(k + " = " + v); }

    static Object viaRef(Reference<?> r) { return r.get(); }

    /** A busy wait: Thread.sleep after System.gc() hangs under --jdk-only on the base binary (not this lane). */
    static void spin(long ms) {
        long t = System.nanoTime() + ms * 1_000_000L;
        while (System.nanoTime() < t) {
            Thread.onSpinWait();
        }
    }

    static long hot(WeakReference<Box> w, SoftReference<Box> s, Box b, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            if (w.get() == b) acc++;
            if (viaRef(s) == b) acc++;
        }
        return acc;
    }

    public static void main(String[] a) throws Exception {
        Box b = new Box(1);
        WeakReference<Box> w = new WeakReference<>(b);
        SoftReference<Box> s = new SoftReference<>(b);
        PhantomReference<Box> p = new PhantomReference<>(b, new ReferenceQueue<>());
        line("weak get", w.get());
        line("soft get", s.get());
        line("phantom get", p.get());
        line("weak same", w.get() == b);
        line("as Reference", viaRef(w) + "," + viaRef(s) + "," + viaRef(p));
        // hot calls: every tier (the loop is in its own method, called repeatedly)
        long acc = 0;
        for (int r = 0; r < 30; r++) {
            acc += hot(w, s, b, 10_000);
        }
        line("hot acc", acc);
        // subclass override
        MyWeak<Box> mw = new MyWeak<>(b);
        line("override get", mw.get() + " gets=" + mw.gets + " via=" + viaRef(mw) + " gets=" + mw.gets);
        // clear
        WeakReference<Box> w2 = new WeakReference<>(b);
        w2.clear();
        line("cleared get", w2.get());
        line("cleared refersTo", w2.refersTo(null));
        // null referent
        line("null referent", new WeakReference<Object>(null).get());
        // queue
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object tmp = new Object();
        WeakReference<Object> w3 = new WeakReference<>(tmp, q);
        line("queued before", q.poll());
        tmp = null;
        for (int i = 0; i < 5 && w3.get() != null; i++) {
            System.gc();
            spin(50);
        }
        line("collected get", w3.get());
        Reference<?> got = null;
        for (int i = 0; i < 40 && got == null; i++) {
            got = q.poll();
            spin(50);
        }
        line("queue delivered", got == w3);
        // WeakHashMap and ThreadLocal behaviour
        WeakHashMap<Object, String> whm = new WeakHashMap<>();
        Object k1 = new Object();
        whm.put(k1, "one");
        whm.put(new Object(), "gone");
        for (int i = 0; i < 5 && whm.size() > 1; i++) {
            System.gc();
            spin(50);
        }
        line("whm size", whm.size() + " k1=" + whm.get(k1));
        ThreadLocal<String> tl = new ThreadLocal<>();
        tl.set("tls");
        line("threadlocal", tl.get());
        tl.remove();
        line("threadlocal removed", tl.get());
        // referent survives a GC while strongly held
        System.gc();
        line("held survives", w.get() == b && s.get() == b);
        line("done", b.v);
    }
}
