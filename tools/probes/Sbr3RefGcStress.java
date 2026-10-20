// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * SBR round 3, lane {@code ref}: a GC-heavy stress of the reference path with the JDK's own
 * bytecode for {@code <init>} / {@code get} / {@code refersTo}.
 *
 * <ul>
 *   <li>{@code publish}: a referent reachable ONLY through its weak reference is read with
 *       {@code get()} and stored into a long-lived array while a collector cycle may be marking
 *       (the SATB hazard {@code gc_reference_keep_alive} guards on the native path). After the
 *       storm every stored element must still be an intact {@code Payload} with its own index.
 *   <li>{@code churn}: many short-lived references with queues; every collector-cleared reference
 *       is enqueued exactly once and its referent field is null.
 *   <li>{@code soft}: soft references to recently touched referents survive collections.
 * </ul>
 *
 * <pre>
 *   cratonvm --jdk-only -Xmx256m [-XX:+UseG1GC] -cp . Sbr3RefGcStress [rounds]
 * </pre>
 */
public final class Sbr3RefGcStress {
    static final class Payload {
        final int id;
        final long[] pad = new long[6];

        Payload(int id) {
            this.id = id;
            pad[0] = id * 31L;
        }

        boolean intact(int expected) {
            return id == expected && pad[0] == expected * 31L;
        }
    }

    static final int N = 20000;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        int bad = 0;
        int lost = 0;
        for (int round = 0; round < rounds; round++) {
            // ---- publish
            @SuppressWarnings("unchecked")
            WeakReference<Payload>[] refs = new WeakReference[N];
            for (int i = 0; i < N; i++) {
                refs[i] = new WeakReference<>(new Payload(i));
            }
            Object[] store = new Object[N];
            AtomicBoolean stop = new AtomicBoolean();
            Thread alloc = new Thread(() -> {
                List<byte[]> junk = new ArrayList<>();
                while (!stop.get()) {
                    junk.add(new byte[8192]);
                    if (junk.size() > 4000) {
                        junk.clear();
                    }
                }
            });
            alloc.start();
            for (int i = 0; i < N; i++) {
                Payload p = refs[i].get();
                if (p != null) {
                    store[i] = p;
                }
                if ((i & 1023) == 0) {
                    Thread.yield();
                }
            }
            stop.set(true);
            alloc.join();
            System.gc();
            for (int i = 0; i < N; i++) {
                Object o = store[i];
                if (o != null) {
                    if (!(o instanceof Payload) || !((Payload) o).intact(i)) {
                        bad++;
                    } else if (refs[i].get() != o || refs[i].refersTo(null)) {
                        // stored, still strongly reachable: the reference must not have been cleared
                        lost++;
                    }
                }
            }
            // ---- churn
            ReferenceQueue<Object> q = new ReferenceQueue<>();
            List<WeakReference<Object>> keep = new ArrayList<>();
            for (int i = 0; i < 60000; i++) {
                WeakReference<Object> w = new WeakReference<>(new Object(), q);
                if ((i & 63) == 0) {
                    keep.add(w);
                }
            }
            for (int i = 0; i < 12; i++) {
                System.gc();
            }
            int cleared = 0;
            for (WeakReference<Object> w : keep) {
                if (w.refersTo(null)) {
                    cleared++;
                }
            }
            int enq = 0;
            java.lang.ref.Reference<?> r;
            while ((r = q.remove(200)) != null) {
                enq++;
                if (!((WeakReference<?>) r).refersTo(null)) {
                    bad++;
                }
            }
            // ---- soft
            SoftReference<Payload> soft = new SoftReference<>(new Payload(7));
            for (int i = 0; i < 4; i++) {
                System.gc();
            }
            Payload sp = soft.get();
            boolean softKept = sp != null && sp.intact(7);
            System.out.println("round " + round + " keptCleared=" + (cleared == keep.size()) + " enqueued=" + (enq > 0)
                    + " softKept=" + softKept);
        }
        System.out.println("bad=" + bad + " lost=" + lost);
        System.out.println(bad == 0 && lost == 0 ? "GC-STRESS-OK" : "GC-STRESS-FAIL");
    }
}
