// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.*;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/** Set-time crossings must deliver while the setting thread sleeps or spins. */
public final class GcSetThresholdDeliveryProbe {
    static volatile Object held;
    static volatile long spin;
    public static void main(String[] args) throws Exception {
        boolean heap = args[0].equals("heap");
        boolean busy = args[1].equals("busy");
        held = new byte[2 << 20];
        // Also give the code-heap variant compiled code to measure.
        for (int i = 0; i < 100000; i++) spin += i;
        System.gc();
        String name = args[0].equals("code") ? "CodeHeap 'non-profiled nmethods'"
                : args[0].equals("classspace") ? "Compressed Class Space" : "Metaspace";
        MemoryPoolMXBean found = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.isUsageThresholdSupported() && (heap ? p.getType() == MemoryType.HEAP
                    : p.getName().equals(name))) found = p;
        }
        if (found == null || found.getUsage().getUsed() <= 0) throw new AssertionError("no used pool");
        final MemoryPoolMXBean pool = found;
        AtomicInteger count = new AtomicInteger();
        AtomicInteger bad = new AtomicInteger();
        Thread setter = Thread.currentThread();
        ((NotificationEmitter) ManagementFactory.getMemoryMXBean()).addNotificationListener((n, h) -> {
            MemoryNotificationInfo info = MemoryNotificationInfo.from((CompositeData) n.getUserData());
            if (!info.getPoolName().equals(pool.getName())) return;
            // Must be asynchronous, and may need the setter's own pool monitor.
            synchronized (pool) {
                if (Thread.currentThread() == setter || !n.getType().equals(
                        MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED)
                        || info.getUsage().getUsed() < 1 || info.getCount() != count.get() + 1) {
                    System.err.println("[threshold] invalid listener sameThread="
                            + (Thread.currentThread() == setter) + " type=" + n.getType()
                            + " used=" + info.getUsage().getUsed() + " count=" + info.getCount()
                            + " expected=" + (count.get() + 1));
                    bad.incrementAndGet();
                }
                count.incrementAndGet();
            }
        }, null, null);
        // Warm this exact callee before setting, so a first compilation of
        // the wait loop cannot itself provide the missing notification door.
        for (int i = 0; i < 100000; i++) spinUntil(count, 0, 0);
        long before = collections();
        long max = pool.getUsage().getMax();
        for (int crossing = 1; crossing <= 2; crossing++) {
            pool.setUsageThreshold(max > 0 ? max : Long.MAX_VALUE);
            Thread.sleep(100);
            System.err.println("[threshold] arm " + crossing);
            pool.setUsageThreshold(1);
            long end = System.nanoTime() + 3_000_000_000L;
            if (busy) {
                spinUntil(count, crossing, end);
            } else {
                while (count.get() < crossing && System.nanoTime() < end) Thread.sleep(10);
            }
            if (count.get() != crossing) throw new AssertionError("undelivered crossing " + crossing);
        }
        Thread.sleep(100);
        long delta = collections() - before;
        System.out.println(args[0] + " " + args[1] + " notified=" + count.get()
                + " count=" + pool.getUsageThresholdCount() + " collections=" + delta + " bad=" + bad.get());
        if (count.get() != 2 || pool.getUsageThresholdCount() != 2 || delta != 0 || bad.get() != 0)
            throw new AssertionError("threshold delivery mismatch");
        pool.setUsageThreshold(0);
        java.lang.ref.Reference.reachabilityFence(held);
    }
    static long collections() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) n += gc.getCollectionCount();
        return n;
    }
    static void spinUntil(AtomicInteger count, int target, long end) {
        while (count.get() < target && System.nanoTime() < end) spin++;
    }
}
