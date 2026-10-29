// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * Every access mode of a real {@code int[]} / {@code long[]} element
 * {@code VarHandle}, driven hot enough to be compiled: the exact shapes the
 * dispatch funnel serves as one atomic instruction, the shapes it must decline
 * (a widening site, an {@code Object} site, a null array, an index out of
 * range), and two threads racing on the same elements. The output is HotSpot's.
 */
public final class VarHandleArrayElementProbe {
    static final VarHandle INTS = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle LONGS = MethodHandles.arrayElementVarHandle(long[].class);
    static final VarHandle BYTES = MethodHandles.arrayElementVarHandle(byte[].class);
    static long sink;

    static long ints(AtomicIntegerArray a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 15;
            a.set(k, i);
            s += a.get(k);
            a.lazySet(k, i + 1);
            s += a.getAcquire(k);
            a.setRelease(k, i + 2);
            a.setPlain(k, i + 3);
            s += a.getPlain(k) + a.getOpaque(k);
            a.setOpaque(k, i + 4);
            s += a.getAndAdd(k, 3);
            s += a.incrementAndGet(k);
            s += a.getAndSet(k, i);
            s += a.compareAndSet(k, i, i + 7) ? 1 : 0;
            s += a.compareAndSet(k, -1, 5) ? 1 : 0;
            s += a.weakCompareAndSetPlain(k, i + 7, i + 8) ? 1 : 0;
            s += a.weakCompareAndSetVolatile(k, i + 8, i + 9) ? 1 : 0;
            s += a.addAndGet(k, -2);
            s += a.getAndIncrement(k);
            s += a.decrementAndGet(k);
        }
        return s;
    }

    static long longs(AtomicLongArray a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 15;
            long big = (1L << 40) + i;
            a.set(k, big);
            s += a.get(k);
            a.lazySet(k, big + 1);
            s += a.getAcquire(k);
            s += a.getAndAdd(k, 3L << 33);
            s += a.incrementAndGet(k);
            s += a.getAndSet(k, big);
            s += a.compareAndSet(k, big, big + 7) ? 1 : 0;
            s += a.compareAndSet(k, -1L, 5L) ? 1 : 0;
            s += a.weakCompareAndSetPlain(k, big + 7, big + 8) ? 1 : 0;
            s += a.addAndGet(k, -2);
        }
        return s;
    }

    /** Exact sites on raw handles, a widening site, and an Object site. */
    static long raw(int[] ia, long[] la, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            INTS.setVolatile(ia, k, i);
            s += (int) INTS.getVolatile(ia, k);
            s += (long) INTS.get(ia, k);
            s += (Integer) INTS.get(ia, k);
            Object o = INTS.getAcquire(ia, k);
            s += ((Integer) o).intValue();
            s += (int) INTS.getAndAdd(ia, k, 1);
            LONGS.set(la, k, (long) i << 32);
            s += (long) LONGS.getOpaque(la, k);
            s += (long) LONGS.getAndAdd(la, k, 1L);
            s += (boolean) LONGS.compareAndSet(la, k, ((long) i << 32) + 1, 9L) ? 1 : 0;
        }
        return s;
    }

    static String report(String what, Runnable r) {
        try {
            r.run();
            return what + ": no exception";
        } catch (Throwable t) {
            return what + ": " + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        AtomicIntegerArray ia = new AtomicIntegerArray(16);
        AtomicLongArray la = new AtomicLongArray(16);
        int[] raw = new int[8];
        long[] rawL = new long[8];
        long s = 0;
        for (int rep = 0; rep < 5; rep++) {
            s += ints(ia, 20_000);
            s += longs(la, 20_000);
            s += raw(raw, rawL, 20_000);
        }
        System.out.println("single=" + s);
        System.out.println("ia=" + ia + " la=" + la);

        // The shapes the fast path declines must still raise HotSpot's exceptions, hot or cold.
        for (int i = 0; i < 3; i++) {
            System.out.println(report("ia.get(16)", () -> ia.get(16)));
            System.out.println(report("ia.set(-1)", () -> ia.set(-1, 1)));
            System.out.println(report("la.getAndAdd(99)", () -> la.getAndAdd(99, 1)));
            System.out.println(report("INTS.get(null)", () -> sink += (int) INTS.get((int[]) null, 0)));
            System.out.println(report("INTS.set(len)", () -> INTS.set(raw, 8, 1)));
            System.out.println(report("INTS.compareAndSet(-3)", () -> sink += (boolean) INTS.compareAndSet(raw, -3, 1, 2) ? 1 : 0));
            System.out.println(report("LONGS.get(len)", () -> sink += (long) LONGS.get(rawL, 8)));
            System.out.println(report("BYTES.get(len)", () -> sink += (byte) BYTES.get(new byte[2], 2)));
        }

        // Two threads on the same elements: no lost update through getAndAdd or a CAS loop.
        AtomicIntegerArray shared = new AtomicIntegerArray(4);
        AtomicLongArray sharedL = new AtomicLongArray(4);
        int per = 200_000;
        Runnable work = () -> {
            for (int i = 0; i < per; i++) {
                shared.getAndIncrement(i & 3);
                shared.incrementAndGet(0);
                sharedL.getAndAdd(i & 3, 1L << 33);
                int k = (i >> 1) & 3;
                int v;
                do {
                    v = shared.get(k);
                } while (!shared.compareAndSet(k, v, v + 1));
                long w;
                do {
                    w = sharedL.get(k);
                } while (!sharedL.compareAndSet(k, w, w + 1));
            }
        };
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(work);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("shared=" + shared + " sharedL=" + sharedL);
        System.out.println("ARRAY-ELEMENT-OK");
    }
}
