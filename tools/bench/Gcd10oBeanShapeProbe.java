// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gcd d10/o retirement (2026-10-04): the SHAPE of the GC and heap-pool
 * MXBeans, beyond {@code Gcd1MxNamesProbe}'s names -- which pools each
 * collector manages, which collectors each heap pool names as its managers,
 * whether each pool's max is defined, which beans a {@code System.gc()}
 * moves, and the (bean, action, cause) of the notifications it sends.
 * {@code docs/internal/gc/gcd-d10o-g1-and-zgc-gc-log-and-mxbean-shapes-differ-from-hotspot-FIXED-20261005.md}.
 *
 * <p>Deterministic: names are sorted, counts are reduced to "moved" flags,
 * notifications to a sorted set of distinct (bean, action, cause) triples
 * with the HEAP pools their {@code GcInfo} carries (HotSpot's also carry its
 * non-heap pools, which this VM has no beans for: a recorded divergence,
 * {@code gengc-r4w3-obs2-...-RETIRED-20260924.md} residual 4). No sizes or
 * times are printed.
 *
 * <p>Run against HotSpot 25 and CratonVM with the same collector flag:
 * <pre>
 *   java     -XX:+UseZGC -Xmx256m -cp out Gcd10oBeanShapeProbe
 *   cratonvm -XX:+UseZGC -Xmx256m -cp out Gcd10oBeanShapeProbe
 * </pre>
 */
public final class Gcd10oBeanShapeProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        TreeSet<String> notes = new TreeSet<>();
        TreeSet<String> heapPools = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                heapPools.add(p.getName());
            }
        }
        for (GarbageCollectorMXBean gc : gcs) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info =
                            GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                    TreeSet<String> pools = new TreeSet<>(info.getGcInfo().getMemoryUsageAfterGc().keySet());
                    pools.retainAll(heapPools);
                    synchronized (notes) {
                        notes.add("note " + info.getGcName() + " | " + info.getGcAction() + " | "
                                + info.getGcCause() + " | heap-pools=" + pools);
                    }
                }
            }, null, null);
        }
        TreeMap<String, Long> before = new TreeMap<>();
        for (GarbageCollectorMXBean gc : gcs) {
            before.put(gc.getName(), Math.max(0, gc.getCollectionCount()));
            System.out.println("collector " + gc.getName() + " pools="
                    + new TreeSet<>(List.of(gc.getMemoryPoolNames())));
        }
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            MemoryUsage u = p.getUsage();
            MemoryUsage cu = p.getCollectionUsage();
            System.out.println("pool " + p.getName() + " managers="
                    + new TreeSet<>(List.of(p.getMemoryManagerNames()))
                    + " max-defined=" + (u.getMax() >= 0)
                    + " init-defined=" + (u.getInit() >= 0)
                    + " collection-usage=" + (cu != null)
                    + " usage-threshold=" + p.isUsageThresholdSupported()
                    + " collection-threshold=" + p.isCollectionUsageThresholdSupported());
        }
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 4096; i++) {
            byte[] b = new byte[1024];
            sink = b;
            if ((i & 63) == 0) {
                keep.add(b);
            }
        }
        System.gc();
        int moved = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long b = before.getOrDefault(gc.getName(), 0L);
            boolean m = gc.getCollectionCount() > b;
            moved += m ? 1 : 0;
            System.out.println("moved " + gc.getName() + "=" + m);
        }
        // Notifications are delivered asynchronously, on HotSpot within tens of
        // milliseconds; this VM's FIRST delivery can take over a second (the
        // delivery thread starts cold). Wait for one per moved bean, bounded.
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (System.nanoTime() < deadline) {
            synchronized (notes) {
                if (notes.size() >= moved) {
                    break;
                }
            }
            Thread.sleep(20);
        }
        Thread.sleep(200);
        synchronized (notes) {
            for (String n : notes) {
                System.out.println(n);
            }
        }
        System.out.println("kept " + keep.size());
    }
}
