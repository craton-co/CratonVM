// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ConcurrentSkipListSet;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gen r5w3/obs7 (2026-09-26): the heap pools' usage and collection-usage
 * thresholds, and the {@code MemoryMXBean} notifications HotSpot sends when
 * they are crossed. Page:
 * {@code docs/internal/gc/gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds-FIXED-20260928.md}.
 *
 * <p>Checks, against HotSpot Serial:
 * <ol>
 *   <li>which heap pools support which threshold ({@code Tenured Gen} both,
 *       {@code Eden Space} / {@code Survivor Space} the collection-usage one);
 *   <li>{@code Eden Space.setUsageThreshold} throws
 *       {@code UnsupportedOperationException};
 *   <li>with a listener on the {@code MemoryMXBean}, a 1-byte usage threshold
 *       and a 1-byte collection-usage threshold on {@code Tenured Gen}, and a
 *       {@code System.gc()}, both notifications arrive
 *       ({@code MEMORY_THRESHOLD_EXCEEDED} and
 *       {@code MEMORY_COLLECTION_THRESHOLD_EXCEEDED}), and the pool's
 *       exceeded flags and counts say so.
 * </ol>
 * Deterministic stdout on HotSpot Serial:
 * <pre>
 *   pool Eden Space usageThresholdSupported=false collectionUsageThresholdSupported=true
 *   pool Survivor Space usageThresholdSupported=false collectionUsageThresholdSupported=true
 *   pool Tenured Gen usageThresholdSupported=true collectionUsageThresholdSupported=true
 *   eden-usage-threshold UnsupportedOperationException
 *   notification java.management.memory.collection.threshold.exceeded Tenured Gen
 *   notification java.management.memory.threshold.exceeded Tenured Gen
 *   tenured-usage-threshold-exceeded=true
 *   tenured-usage-threshold-count-positive=true
 *   tenured-collection-threshold-exceeded=true
 *   tenured-collection-threshold-count-positive=true
 *   PASS
 * </pre>
 * Before gen r5w3/obs7 CratonVM printed {@code false} for every support flag,
 * no notification, the four {@code =false} lines and {@code FAIL}. Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx128m -cp tools/bench GenR5W3PoolThresholdProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR5W3PoolThresholdProbe
 * </pre>
 * A usage-threshold crossing is detected at a collection's end (and when the
 * threshold is set), and delivered at the next GC-notification drain point on
 * CratonVM; HotSpot's Service Thread delivers it asynchronously. The probe
 * collects until both have arrived (at most 10 s).
 */
public final class GenR5W3PoolThresholdProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        List<MemoryPoolMXBean> heap = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                heap.add(p);
            }
        }
        heap.sort(Comparator.comparing(MemoryPoolMXBean::getName));
        MemoryPoolMXBean tenured = null;
        MemoryPoolMXBean eden = null;
        for (MemoryPoolMXBean p : heap) {
            System.out.println("pool " + p.getName()
                + " usageThresholdSupported=" + p.isUsageThresholdSupported()
                + " collectionUsageThresholdSupported=" + p.isCollectionUsageThresholdSupported());
            if (p.getName().equals("Tenured Gen")) {
                tenured = p;
            } else if (p.getName().equals("Eden Space")) {
                eden = p;
            }
        }
        if (tenured == null || eden == null) {
            System.out.println("FAIL not Serial's pools");
            return;
        }
        try {
            eden.setUsageThreshold(1);
            System.out.println("eden-usage-threshold accepted");
        } catch (UnsupportedOperationException e) {
            System.out.println("eden-usage-threshold UnsupportedOperationException");
        }

        Set<String> seen = new ConcurrentSkipListSet<>();
        CountDownLatch both = new CountDownLatch(2);
        NotificationEmitter emitter = (NotificationEmitter) ManagementFactory.getMemoryMXBean();
        emitter.addNotificationListener((n, hb) -> {
            String type = n.getType();
            if (MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED.equals(type)
                    || MemoryNotificationInfo.MEMORY_COLLECTION_THRESHOLD_EXCEEDED.equals(type)) {
                MemoryNotificationInfo info = MemoryNotificationInfo.from((CompositeData) n.getUserData());
                if (seen.add(type + " " + info.getPoolName())) {
                    both.countDown();
                }
            }
        }, null, null);

        if (tenured.isCollectionUsageThresholdSupported()) {
            tenured.setCollectionUsageThreshold(1);
        }
        if (tenured.isUsageThresholdSupported()) {
            tenured.setUsageThreshold(1);
        }
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            keep.add(new byte[64 * 1024]);
        }
        sink = keep;
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (both.getCount() > 0 && System.nanoTime() < deadline) {
            System.gc();
            both.await(200, TimeUnit.MILLISECONDS);
        }
        for (String s : seen) {
            System.out.println("notification " + s);
        }
        boolean usage = tenured.isUsageThresholdSupported();
        boolean collection = tenured.isCollectionUsageThresholdSupported();
        System.out.println("tenured-usage-threshold-exceeded="
            + (usage && tenured.isUsageThresholdExceeded()));
        System.out.println("tenured-usage-threshold-count-positive="
            + (usage && tenured.getUsageThresholdCount() >= 1));
        System.out.println("tenured-collection-threshold-exceeded="
            + (collection && tenured.isCollectionUsageThresholdExceeded()));
        System.out.println("tenured-collection-threshold-count-positive="
            + (collection && tenured.getCollectionUsageThresholdCount() >= 1));
        System.out.println(both.getCount() == 0 ? "PASS" : "FAIL");
    }
}
