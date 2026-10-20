// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.atomic.AtomicLong;

/**
 * zr z8/st: objects published by their allocating thread while its TLAB chunk is still live,
 * then used by another thread -- the cross-thread window of a start registry without a
 * per-allocation bit (CRATONVM_ZGC_LAZY_TLAB_STARTS). Producers allocate small arrays and
 * holder objects and hand them over a queue; consumers read them, cast them, store them into a
 * shared ring (so they stay live across collections) and sum their contents. A warm-up phase
 * gets every method compiled first.
 *
 * <p>Usage: {@code Zr8StPeerHandoff <producers> <millions-of-handoffs> [ringSize]}. Prints one
 * {@code check} line that must equal HotSpot's; the timing line is informational.
 */
public final class Zr8StPeerHandoff {
    static final class Holder {
        final int[] payload;
        final long tag;
        Holder next;

        Holder(int[] payload, long tag) {
            this.payload = payload;
            this.tag = tag;
        }
    }

    static Object make(long i) {
        int n = (int) (i & 15) + 1;
        int[] a = new int[n];
        for (int k = 0; k < n; k++) {
            a[k] = (int) (i + k);
        }
        if ((i & 3) == 0) {
            return a;
        }
        return new Holder(a, i);
    }

    static long use(Object o, Object[] ring, int slot) {
        ring[slot] = o;
        if (o instanceof Holder h) {
            long s = h.tag;
            for (int v : h.payload) {
                s += v;
            }
            Object prev = ring[(slot + 1) % ring.length];
            if (prev instanceof Holder p) {
                h.next = p;
            }
            return s;
        }
        int[] a = (int[]) o;
        long s = 0;
        for (int v : a) {
            s += v;
        }
        return s;
    }

    static long run(int producers, long total, int ringSize) throws InterruptedException {
        ArrayBlockingQueue<Object> q = new ArrayBlockingQueue<>(1024);
        AtomicLong sum = new AtomicLong();
        long perProducer = total / producers;
        Thread[] ps = new Thread[producers];
        for (int p = 0; p < producers; p++) {
            final long base = (long) p * perProducer;
            ps[p] = new Thread(() -> {
                try {
                    for (long i = 0; i < perProducer; i++) {
                        q.put(make(base + i));
                    }
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            });
            ps[p].start();
        }
        Object[] ring = new Object[ringSize];
        long s = 0;
        long expected = perProducer * producers;
        for (long i = 0; i < expected; i++) {
            Object o = q.take();
            s += use(o, ring, (int) (i % ringSize));
        }
        for (Thread t : ps) {
            t.join();
        }
        sum.addAndGet(s);
        return sum.get();
    }

    public static void main(String[] args) throws Exception {
        int producers = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        long millions = args.length > 1 ? Long.parseLong(args[1]) : 4;
        int ringSize = args.length > 2 ? Integer.parseInt(args[2]) : 4096;
        long warm = run(producers, 200_000, ringSize);
        long t0 = System.nanoTime();
        long s = run(producers, millions * 1_000_000L, ringSize);
        long t1 = System.nanoTime();
        System.out.println("check producers=" + producers + " handoffs=" + millions + "M warm=" + warm
                + " sum=" + s);
        System.err.println("ns_per_handoff=" + (t1 - t0) / (millions * 1_000_000L));
    }
}
