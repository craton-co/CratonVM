// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentLinkedQueue;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zr z9/hp (2026-10-13): the GC MXBean surface of HotSpot 25 ZGC reduced to
 * what must hold on BOTH VMs -- names, orders, joins and count RELATIONS --
 * so a SAME row is meaningful although the absolute pause counts differ by
 * design (HotSpot books 8 pauses per {@code System.gc()}, this VM one per
 * stop-the-world collection: {@code zgc-zr1tr-pause-beans-count-one-pause-per-cycle-20261012.md}).
 *
 * <p>Run {@code -XX:+UseZGC -Xmx256m Zr9HpBeanRelations}. stdout lines
 * (deterministic):
 * <ul>
 * <li>{@code collectors [..]}: {@code getGarbageCollectorMXBeans()} order;</li>
 * <li>{@code collector <name> pools=[..]} per collector;</li>
 * <li>{@code heap_pools [..]} and {@code pool <name> managers=[..]};</li>
 * <li>after {@code n} {@code System.gc()} calls on a quiet heap:
 *     {@code major_cycles_delta=n}, {@code minor_cycles_delta=0},
 *     {@code minor_pauses_delta=0}, {@code major_pauses_per_cycle_ge_1=true},
 *     {@code pause_notifications_match_count=true} (one notification per
 *     booked pause) and {@code cycle_notifications_match_count=true};</li>
 * <li>{@code sysgc_causes [..]}: distinct {@code bean|action|cause};</li>
 * <li>{@code gcinfo_heap_pools [..]}: the heap pools in the last GcInfo;</li>
 * <li>{@code heap_committed_is_pool_sum=true}, {@code max_is_xmx=true};</li>
 * <li>{@code done}.</li>
 * </ul>
 * stderr: the raw counts.
 */
public final class Zr9HpBeanRelations {
    static Object keep;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        keep = new byte[1 << 20];
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        List<String> names = new ArrayList<>();
        Map<String, Long> before = new HashMap<>();
        ConcurrentLinkedQueue<String> notes = new ConcurrentLinkedQueue<>();
        for (GarbageCollectorMXBean gc : gcs) {
            names.add(gc.getName());
            System.out.println("collector " + gc.getName() + " pools=" + Arrays.toString(gc.getMemoryPoolNames()));
            ((NotificationEmitter) gc).addNotificationListener((note, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(note.getType())) {
                    GarbageCollectionNotificationInfo info =
                            GarbageCollectionNotificationInfo.from((CompositeData) note.getUserData());
                    notes.add(info.getGcName() + "|" + info.getGcAction() + "|" + info.getGcCause());
                }
            }, null, null);
        }
        System.out.println("collectors " + names);
        List<String> heapPools = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                heapPools.add(p.getName());
                System.out.println("pool " + p.getName() + " managers=" + Arrays.toString(p.getMemoryManagerNames()));
            }
        }
        System.out.println("heap_pools " + heapPools);
        for (GarbageCollectorMXBean gc : gcs) {
            before.put(gc.getName(), gc.getCollectionCount());
        }
        for (int i = 0; i < n; i++) {
            System.gc();
        }
        // Notifications are delivered on their own thread: wait until every
        // booked collection has one, or 5 s.
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (System.nanoTime() < deadline) {
            long booked = 0;
            for (GarbageCollectorMXBean gc : gcs) {
                booked += gc.getCollectionCount() - before.get(gc.getName());
            }
            if (notes.size() >= booked && booked > 0) {
                break;
            }
            Thread.sleep(20);
        }
        Thread.sleep(200);
        Map<String, Long> delta = new HashMap<>();
        for (GarbageCollectorMXBean gc : gcs) {
            delta.put(gc.getName(), gc.getCollectionCount() - before.get(gc.getName()));
        }
        Map<String, Integer> perBean = new HashMap<>();
        TreeSet<String> causes = new TreeSet<>();
        for (String t : notes) {
            perBean.merge(t.substring(0, t.indexOf('|')), 1, Integer::sum);
            causes.add(t);
        }
        System.err.println("[probe] delta=" + delta + " notifications=" + perBean);
        long majorCycles = delta.getOrDefault("ZGC Major Cycles", -1L);
        long majorPauses = delta.getOrDefault("ZGC Major Pauses", -1L);
        System.out.println("major_cycles_delta=" + majorCycles);
        System.out.println("minor_cycles_delta=" + delta.getOrDefault("ZGC Minor Cycles", -1L));
        System.out.println("minor_pauses_delta=" + delta.getOrDefault("ZGC Minor Pauses", -1L));
        System.out.println("major_pauses_per_cycle_ge_1=" + (majorCycles > 0 && majorPauses >= majorCycles));
        System.out.println("pause_notifications_match_count="
                + (perBean.getOrDefault("ZGC Major Pauses", 0) == majorPauses));
        System.out.println("cycle_notifications_match_count="
                + (perBean.getOrDefault("ZGC Major Cycles", 0) == majorCycles));
        System.out.println("sysgc_causes " + causes);
        TreeSet<String> infoPools = new TreeSet<>();
        for (GarbageCollectorMXBean gc : gcs) {
            if (gc.getName().equals("ZGC Major Cycles") && gc instanceof com.sun.management.GarbageCollectorMXBean) {
                com.sun.management.GcInfo gi = ((com.sun.management.GarbageCollectorMXBean) gc).getLastGcInfo();
                if (gi != null) {
                    for (String k : gi.getMemoryUsageAfterGc().keySet()) {
                        if (heapPools.contains(k)) {
                            infoPools.add(k);
                        }
                    }
                }
            }
        }
        System.out.println("gcinfo_heap_pools " + infoPools);
        long poolCommitted = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                poolCommitted += p.getUsage().getCommitted();
            }
        }
        MemoryUsage heap = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        System.err.println("[probe] heap committed=" + heap.getCommitted() + " pools committed=" + poolCommitted
                + " max=" + heap.getMax() + " runtime max=" + Runtime.getRuntime().maxMemory());
        System.out.println("heap_committed_is_pool_sum=" + (heap.getCommitted() == poolCommitted));
        System.out.println("max_is_xmx=" + (Runtime.getRuntime().maxMemory() == 256L << 20
                && heap.getMax() == 256L << 20));
        System.out.println("done");
    }
}
