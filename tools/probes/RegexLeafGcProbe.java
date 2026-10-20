// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicReference;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Concurrent collections during compiled regex/parse loops, with retained captures. */
public class RegexLeafGcProbe {
    static final String[][] keep = new String[4][1000];
    static final AtomicReference<Throwable> failure = new AtomicReference<>();

    static long gcCount() {
        long count = 0;
        for (var bean : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(0, bean.getCollectionCount());
        }
        return count;
    }

    static long consume(Matcher matcher, int id, int rounds) {
        long sum = 0;
        for (int i = 0; i < rounds; i++) {
            matcher.reset();
            while (matcher.find()) {
                String value = matcher.group(1);
                sum += Long.parseLong(value);
                keep[id][i % keep[id].length] = value;
            }
        }
        return sum;
    }

    static long kernel(int id, int rounds, CountDownLatch ready, CountDownLatch start) throws Exception {
        // The worker starts with another thread's String and a cold TLS layout.
        if (Long.parseLong("17") != 17) throw new AssertionError("cold parse");
        Matcher matcher = Pattern.compile(id % 2 == 0 ? "(\\d+)" : "([0-9]+)").matcher("11 22 333");
        if (consume(matcher, id, 5000) != 5000L * 366) throw new AssertionError("warm checksum");
        ready.countDown();
        start.await();
        long sum = consume(matcher, id, rounds);
        for (String value : keep[id]) {
            if (!"333".equals(value)) throw new AssertionError("retained capture");
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length == 0 ? 20_000 : Integer.parseInt(args[0]);
        gcCount(); // initialize the management path before worker warmup
        CountDownLatch ready = new CountDownLatch(4);
        CountDownLatch start = new CountDownLatch(1);
        long[] sums = new long[4];
        Thread[] workers = new Thread[4];
        for (int id = 0; id < workers.length; id++) {
            final int slot = id;
            workers[id] = new Thread(() -> {
                try { sums[slot] = kernel(slot, rounds, ready, start); }
                catch (Throwable error) { failure.compareAndSet(null, error); ready.countDown(); }
            });
            workers[id].start();
        }
        ready.await();
        long before = gcCount();
        Thread collector = new Thread(() -> {
            try {
                start.await();
                for (int i = 0; i < 20; i++) { System.gc(); Thread.yield(); }
            } catch (Throwable error) { failure.compareAndSet(null, error); }
        });
        collector.start();
        start.countDown();
        for (Thread worker : workers) worker.join();
        collector.join();
        if (failure.get() != null) throw new AssertionError(failure.get());
        long sum = 0;
        for (long value : sums) sum += value;
        if (sum != 4L * rounds * 366) throw new AssertionError("checksum " + sum);
        for (String[] row : keep) {
            for (String value : row) {
                if (!"333".equals(value)) throw new AssertionError("capture after final GC");
            }
        }
        if (gcCount() <= before) throw new AssertionError("no completed collection");
        System.out.println("workers=4 gc-requests=20 gc-progress=true retained=4000 checksum=" + sum);
    }
}
