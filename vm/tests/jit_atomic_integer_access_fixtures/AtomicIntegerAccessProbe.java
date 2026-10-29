// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.CountDownLatch;

public final class AtomicIntegerAccessProbe {
    static volatile long sink;
    static int increment(AtomicInteger counter) { return counter.getAndIncrement(); }
    static int read(AtomicInteger counter) { return counter.get(); }
    static void warm(AtomicInteger counter) {
        long sum = 0;
        for (int i = 0; i < 10000; i++) { sum += increment(counter); sum += read(counter); }
        sink = sum;
    }
    public static void main(String[] args) throws Exception {
        AtomicInteger counter = new AtomicInteger();
        warm(counter);
        if (read(counter) != 10000 || sink != 100000000L) throw new AssertionError("warm");
        counter.set(Integer.MAX_VALUE);
        if (increment(counter) != Integer.MAX_VALUE || read(counter) != Integer.MIN_VALUE) {
            throw new AssertionError("overflow");
        }
        counter.set(-1);
        if (increment(counter) != -1 || read(counter) != 0) throw new AssertionError("negative");
        boolean failed = false;
        try { increment(null); } catch (NullPointerException expected) { failed = true; }
        if (!failed) throw new AssertionError("null increment");
        failed = false;
        try { read(null); } catch (NullPointerException expected) { failed = true; }
        if (!failed) throw new AssertionError("null read");
        counter = new AtomicInteger(123456789);
        System.gc();
        if (increment(counter) != 123456789 || read(counter) != 123456790) {
            throw new AssertionError("moving receiver");
        }
        final AtomicInteger shared = new AtomicInteger();
        final CountDownLatch ready = new CountDownLatch(4), start = new CountDownLatch(1);
        final long[] sums = new long[4];
        final Throwable[] failures = new Throwable[4];
        Thread[] workers = new Thread[4];
        for (int t = 0; t < workers.length; t++) {
            final int slot = t;
            workers[t] = new Thread(() -> {
                try {
                    ready.countDown(); start.await();
                    long total = 0;
                    for (int i = 0; i < 10000; i++) total += increment(shared);
                    sums[slot] = total;
                } catch (Throwable failure) { failures[slot] = failure; }
            });
            workers[t].start();
        }
        ready.await(); start.countDown();
        for (Thread worker : workers) worker.join();
        long sum = 0;
        for (int t = 0; t < workers.length; t++) {
            if (failures[t] != null) throw new AssertionError(failures[t]);
            sum += sums[t];
        }
        if (read(shared) != 40000 || sum != 799980000L) throw new AssertionError("lost atomic update");
        System.out.println("ATOMIC-INTEGER-ACCESS-OK");
    }
}
