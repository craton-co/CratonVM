// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;

/**
 * zr z1/tr (2026-10-12): how many times ONE {@code System.gc()} moves each
 * ZGC bean's {@code getCollectionCount()}, and how many notifications each
 * bean sends for it.
 *
 * <p>HotSpot 25's generational ZGC books every stop-the-world pause of a
 * cycle on the {@code ... Pauses} bean (a major collection runs a young and
 * an old phase, each with its own mark-start / mark-end / relocate-start
 * pauses), so its {@code ZGC Major Pauses} count is expected to move by
 * MORE THAN ONE per {@code System.gc()} -- that number has NOT been observed
 * yet; this probe is the measurement. CratonVM books one cycle and one pause
 * (`gc_metrics::BackendBeanShape::collection_beans`).
 *
 * <p>stdout: per bean, {@code <name> count_delta=<n> notifications=<m>} over
 * three {@code System.gc()} calls (sorted), then {@code done}. A row is SAME
 * against {@code java -XX:+UseZGC} or it is a finding.
 */
public final class Zr1TrPauseCountProbe {
    public static void main(String[] args) throws Exception {
        Map<String, AtomicInteger> notes = new ConcurrentHashMap<>();
        Map<String, Long> before = new TreeMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            before.put(gc.getName(), gc.getCollectionCount());
            notes.put(gc.getName(), new AtomicInteger());
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo
                            .from((javax.management.openmbean.CompositeData) n.getUserData());
                    notes.computeIfAbsent(info.getGcName(), k -> new AtomicInteger()).incrementAndGet();
                }
            }, null, null);
        }
        for (int i = 0; i < 3; i++) {
            System.gc();
        }
        Thread.sleep(3000); // the first delivery of a run is slow (gcd-d10o)
        Map<String, String> rows = new TreeMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long delta = gc.getCollectionCount() - before.getOrDefault(gc.getName(), 0L);
            rows.put(gc.getName(), gc.getName() + " count_delta=" + delta
                    + " notifications=" + notes.getOrDefault(gc.getName(), new AtomicInteger()).get());
        }
        rows.values().forEach(System.out::println);
        System.out.println("done");
    }
}
