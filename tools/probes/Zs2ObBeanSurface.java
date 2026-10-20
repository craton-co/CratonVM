// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryManagerMXBean;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentLinkedQueue;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s2/ob (2026-10-14): the Java-visible GC MXBean surface of HotSpot 25 ZGC
 * that does NOT depend on heap layout or timing -- signs, equalities and names
 * of {@code Runtime}, {@code MemoryMXBean}, the two heap pools, each
 * collector's {@code GcInfo} and its notifications. Absolute byte figures are
 * never printed on stdout (HotSpot's {@code used} is page-granular, this VM's
 * byte-granular: {@code zgc-zr2t2-heap-used-is-byte-granular-not-page-granular-CLOSED-20261019.md}).
 * Only HEAP pools are compared: this VM models no non-heap pools
 * ({@code zgc-zs2ob-non-heap-memory-pools-are-not-modelled-FIXED-20261019.md}); their
 * names go to stderr.
 *
 * <p>Run {@code -XX:+UseZGC -Xmx256m Zs2ObBeanSurface}. HotSpot 25.0.3 prints
 * the same stdout on every run (local, 3 runs, md5-stable). stderr: raw figures.
 *
 * <p>Lines that this lane's switches decide (the positive / kill controls):
 * <ul>
 * <li>{@code pool ... init=}: HotSpot's young pool {@code init} is
 *     {@code MinHeapSize}, the old pool's 0 ({@code CRATONVM_ZGC_HOTSPOT_POOL_INIT});</li>
 * <li>{@code collector ZGC Major Pauses ... gcinfo_usage=zero}: HotSpot's pause
 *     manager records no pool usage ({@code CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE});</li>
 * <li>{@code peak_sum_gt_used_sum=true}: HotSpot samples each pool's peak at the
 *     collection's edges ({@code CRATONVM_ZGC_POOL_PEAK_AT_GC}).</li>
 * </ul>
 */
public final class Zs2ObBeanSurface {
    static Object keep;
    static Object sink;

    static String sign(long v) {
        return v < 0 ? "neg" : v == 0 ? "zero" : "pos";
    }

    /** "zero" when every heap pool's usage in {@code m} is all zeros, "set" otherwise. */
    static String heapUsage(Map<String, MemoryUsage> m, List<String> heapPools) {
        for (String p : heapPools) {
            MemoryUsage u = m.get(p);
            if (u == null) {
                return "missing";
            }
            if (u.getInit() != 0 || u.getUsed() != 0 || u.getCommitted() != 0 || u.getMax() != 0) {
                return "set";
            }
        }
        return "zero";
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2;
        long xmx = 256L << 20;
        keep = new byte[1 << 20];

        List<String> managers = new ArrayList<>();
        for (MemoryManagerMXBean m : ManagementFactory.getMemoryManagerMXBeans()) {
            managers.add(m.getName());
        }
        System.err.println("[probe] managers " + managers);

        List<String> heapPools = new ArrayList<>();
        List<String> nonHeapPools = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                nonHeapPools.add(p.getName());
                continue;
            }
            heapPools.add(p.getName());
            MemoryUsage u = p.getUsage();
            System.out.println("pool " + p.getName()
                    + " valid=" + p.isValid()
                    + " init=" + sign(u.getInit())
                    + " max_is_xmx=" + (u.getMax() == xmx)
                    + " committed_le_max=" + (u.getCommitted() <= u.getMax())
                    + " used_le_committed=" + (u.getUsed() <= u.getCommitted())
                    + " usage_threshold=" + p.isUsageThresholdSupported()
                    + " collection_threshold=" + p.isCollectionUsageThresholdSupported()
                    + " collection_usage_before_gc=" + (p.getCollectionUsage() == null ? "null" : "set"));
            System.err.println("[probe] pool " + p.getName() + " " + u);
        }
        System.err.println("[probe] non-heap pools " + nonHeapPools);

        MemoryUsage heap = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        Runtime rt = Runtime.getRuntime();
        long poolInit = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                poolInit += p.getUsage().getInit();
            }
        }
        System.err.println("[probe] heap " + heap + " pool init sum=" + poolInit + " rt total="
                + rt.totalMemory() + " free=" + rt.freeMemory() + " max=" + rt.maxMemory());
        System.out.println("heap init=" + sign(heap.getInit())
                + " pool_init_sum_le_heap_init=" + (poolInit <= heap.getInit())
                + " max_is_xmx=" + (heap.getMax() == xmx)
                + " rt_max_is_xmx=" + (rt.maxMemory() == xmx)
                + " rt_free_le_total=" + (rt.freeMemory() <= rt.totalMemory())
                + " rt_total_le_max=" + (rt.totalMemory() <= rt.maxMemory()));

        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        ConcurrentLinkedQueue<GarbageCollectionNotificationInfo> notes = new ConcurrentLinkedQueue<>();
        long[] count0 = new long[gcs.size()];
        for (int i = 0; i < gcs.size(); i++) {
            GarbageCollectorMXBean gc = gcs.get(i);
            count0[i] = gc.getCollectionCount();
            ((NotificationEmitter) gc).addNotificationListener((note, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(note.getType())) {
                    notes.add(GarbageCollectionNotificationInfo.from((CompositeData) note.getUserData()));
                }
            }, null, null);
        }
        // 16 MiB of garbage before the collections: the pools' pre-collection
        // usage is then well above their post-collection usage on both VMs,
        // and it stays below the 10 % warm-up step at -Xmx256m (25.6 MiB), so
        // no Warmup collection runs first on either VM.
        for (int i = 0; i < 16; i++) {
            sink = new byte[1 << 20];
        }
        sink = null;
        for (int i = 0; i < n; i++) {
            System.gc();
        }
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (System.nanoTime() < deadline) {
            long booked = 0;
            for (int i = 0; i < gcs.size(); i++) {
                booked += gcs.get(i).getCollectionCount() - count0[i];
            }
            if (notes.size() >= booked && booked > 0) {
                break;
            }
            Thread.sleep(20);
        }
        Thread.sleep(200);

        for (int i = 0; i < gcs.size(); i++) {
            GarbageCollectorMXBean gc = gcs.get(i);
            long dc = gc.getCollectionCount() - count0[i];
            String line = "collector " + gc.getName() + " valid=" + gc.isValid() + " moved=" + (dc > 0)
                    + " time_ge_0=" + (gc.getCollectionTime() >= 0);
            GcInfo gi = ((com.sun.management.GarbageCollectorMXBean) gc).getLastGcInfo();
            if (gi == null) {
                line += " gcinfo=null";
            } else {
                TreeSet<String> heapKeys = new TreeSet<>();
                for (String k : gi.getMemoryUsageAfterGc().keySet()) {
                    if (heapPools.contains(k)) {
                        heapKeys.add(k);
                    }
                }
                line += " gcinfo_id_is_count=" + (gi.getId() == gc.getCollectionCount())
                        + " gcinfo_end_ge_start=" + (gi.getEndTime() >= gi.getStartTime())
                        + " gcinfo_duration_ok=" + (gi.getDuration() == gi.getEndTime() - gi.getStartTime())
                        + " gcinfo_keys_same="
                        + gi.getMemoryUsageBeforeGc().keySet().equals(gi.getMemoryUsageAfterGc().keySet())
                        + " gcinfo_heap_keys=" + heapKeys
                        + " gcinfo_usage_before=" + heapUsage(gi.getMemoryUsageBeforeGc(), heapPools)
                        + " gcinfo_usage_after=" + heapUsage(gi.getMemoryUsageAfterGc(), heapPools);
                // HotSpot's GcThreadCount moves with the CPU count and flags
                // (8 CPUs: 11; ParallelGCThreads=3: 12): stderr only.
                System.err.println("[probe] " + gc.getName() + " GcThreadCount=" + gi.get("GcThreadCount"));
                for (Map.Entry<String, MemoryUsage> e : gi.getMemoryUsageAfterGc().entrySet()) {
                    System.err.println("[probe] " + gc.getName() + " after " + e.getKey() + " " + e.getValue());
                }
            }
            System.out.println(line);
            System.err.println("[probe] " + gc.getName() + " dc=" + dc + " time=" + gc.getCollectionTime());
        }

        // The cause is left out of the shape: whether a Warmup collection runs
        // before the System.gc() calls depends on page-granular usage on
        // HotSpot (one does locally at -Xmx256m); the sysgc cause is checked
        // on its own line.
        TreeSet<String> shapes = new TreeSet<>();
        boolean sysgcSeen = false;
        for (GarbageCollectionNotificationInfo info : notes) {
            GcInfo gi = info.getGcInfo();
            sysgcSeen |= "System.gc()".equals(info.getGcCause());
            System.err.println("[probe] notification " + info.getGcName() + "|" + info.getGcCause());
            shapes.add(info.getGcName() + "|" + info.getGcAction()
                    + "|before=" + heapUsage(gi.getMemoryUsageBeforeGc(), heapPools)
                    + "|after=" + heapUsage(gi.getMemoryUsageAfterGc(), heapPools)
                    + "|dur_ok=" + (gi.getDuration() == gi.getEndTime() - gi.getStartTime()));
        }
        System.out.println("notifications " + shapes);
        System.out.println("sysgc_cause_seen=" + sysgcSeen);

        long peakSum = 0;
        long usedSum = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            MemoryUsage cu = p.getCollectionUsage();
            MemoryUsage peak = p.getPeakUsage();
            MemoryUsage u = p.getUsage();
            peakSum += peak.getUsed();
            usedSum += u.getUsed();
            System.out.println("pool_after " + p.getName()
                    + " collection_usage=" + (cu == null ? "null" : "set")
                    + " cu_max_is_xmx=" + (cu != null && cu.getMax() == xmx)
                    + " peak_used_ge_used=" + (peak.getUsed() >= u.getUsed())
                    + " peak_committed_ge_committed=" + (peak.getCommitted() >= u.getCommitted())
                    + " peak_max_is_xmx=" + (peak.getMax() == xmx));
            System.err.println("[probe] pool_after " + p.getName() + " u=" + u + " cu=" + cu + " peak=" + peak);
        }
        System.err.println("[probe] peak sum=" + peakSum + " used sum=" + usedSum);
        System.out.println("peak_sum_gt_used_sum=" + (peakSum > usedSum));
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                long before = p.getPeakUsage().getUsed();
                p.resetPeakUsage();
                System.out.println("pool_reset " + p.getName() + " peak_not_above_before="
                        + (p.getPeakUsage().getUsed() <= before));
            }
        }
        MemoryUsage heap2 = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        long poolCommitted = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                poolCommitted += p.getUsage().getCommitted();
            }
        }
        System.out.println("heap_after committed_is_pool_sum=" + (heap2.getCommitted() == poolCommitted)
                + " rt_total_is_committed=" + (rt.totalMemory() == heap2.getCommitted()));
        System.out.println("done");
    }
}
