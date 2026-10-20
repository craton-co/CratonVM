// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gci i14/mp -- the heap memory-pool and GC MXBean NUMBERS, as booleans a
 * HotSpot 25 run prints identically (Serial for the default collector, G1 with
 * {@code -XX:+UseG1GC}). Page:
 * {@code docs/known-issues/gc/gci-i14mp-memory-pool-and-gc-mxbean-review-findings-20261013.md}.
 *
 * <p>Modes (first argument):
 * <ul>
 * <li>{@code cu0} -- before any collection, every heap pool's
 *     {@code getCollectionUsage()} is HotSpot's {@code (init, 0, 0, max)}
 *     (finding 2). stderr names how many collections ran before (the row is
 *     VOID when that is not 0).
 * <li>{@code peak} -- {@code getPeakUsage()} keeps the high of the old pool
 *     after it shrinks, the eden peak is above eden's usage right after
 *     {@code System.gc()}, and {@code resetPeakUsage()} lowers the peak to the
 *     usage at the reset (finding 1).
 * <li>{@code info} -- {@code getLastGcInfo()} of every bean that ran: its id
 *     within the bean's count, every heap pool in both usage maps, ordered
 *     times, HotSpot's {@code GcThreadCount} attribute (soundness).
 * <li>{@code humgrowth} -- G1 only: a usage threshold on {@code G1 Old Gen}
 *     crossed by humongous allocation alone, with no collection (finding 4,
 *     open: recorded, not judged).
 * <li>{@code cu1} -- young-only collections leave the old pool's collection
 *     usage at {@code (init, 0, 0, max)} (finding 3).
 * <li>{@code g1commit} -- G1 eden's committed after a collection (finding 5,
 *     recorded).
 * <li>{@code cycle} -- one G1 concurrent cycle's beans and notifications
 *     (finding 9; HotSpot with {@code -XX:+ExplicitGCInvokesConcurrent}).
 * </ul>
 *
 * <p>stdout is deterministic booleans; sizes and counts go to stderr.
 */
public final class GciMp14PoolNumbersProbe {
    static volatile Object sink;

    static long totalCollections() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, gc.getCollectionCount());
        }
        return n;
    }

    static TreeMap<String, MemoryPoolMXBean> heapPools() {
        TreeMap<String, MemoryPoolMXBean> m = new TreeMap<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                m.put(p.getName(), p);
            }
        }
        return m;
    }

    static MemoryPoolMXBean find(TreeMap<String, MemoryPoolMXBean> pools, String a, String b) {
        MemoryPoolMXBean p = pools.get(a);
        return p != null ? p : pools.get(b);
    }

    /** Short-lived garbage only: 4 KiB arrays, {@code bytes} in all. */
    static void churn(long bytes) {
        for (long done = 0; done < bytes; done += 4096) {
            sink = new byte[4096];
        }
        sink = null;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "peak";
        System.out.println("mode " + mode);
        if (mode.equals("cu0")) {
            cu0();
        } else if (mode.equals("peak")) {
            peak();
        } else if (mode.equals("info")) {
            info();
        } else if (mode.equals("humgrowth")) {
            humgrowth();
        } else if (mode.equals("cu1")) {
            cu1();
        } else if (mode.equals("g1commit")) {
            g1commit();
        } else if (mode.equals("cycle")) {
            cycle();
        } else {
            System.out.println("unknown mode");
        }
        System.out.println("DONE");
    }

    static void cu0() {
        long before = totalCollections();
        System.err.println("[gci-mp14] cu0 collections-before=" + before);
        for (MemoryPoolMXBean p : heapPools().values()) {
            MemoryUsage cu = p.getCollectionUsage();
            if (cu == null) {
                System.out.println("cu0 " + p.getName() + " null");
                continue;
            }
            System.out.println("cu0 " + p.getName() + " used-zero=" + (cu.getUsed() == 0)
                    + " committed-zero=" + (cu.getCommitted() == 0));
            System.err.println("[gci-mp14] cu0 " + p.getName() + " " + cu);
        }
    }

    static void peak() throws Exception {
        TreeMap<String, MemoryPoolMXBean> pools = heapPools();
        MemoryPoolMXBean eden = find(pools, "Eden Space", "G1 Eden Space");
        MemoryPoolMXBean old = find(pools, "Tenured Gen", "G1 Old Gen");
        if (eden == null || old == null) {
            System.out.println("peak no eden or old pool");
            return;
        }
        List<byte[]> hold = new ArrayList<>();
        for (int i = 0; i < 24; i++) {
            hold.add(new byte[1 << 20]);
        }
        System.gc();
        long oldHigh = old.getUsage().getUsed();
        long oldPeak1 = old.getPeakUsage().getUsed();
        System.out.println("peak old peak covers the old usage: " + (oldPeak1 >= oldHigh));
        System.err.println("[gci-mp14] peak held=" + hold.size() + " oldHigh=" + oldHigh
                + " oldPeak1=" + oldPeak1);
        hold = null;
        churn(32L << 20);
        System.gc();
        long oldLow = old.getUsage().getUsed();
        long oldPeak2 = old.getPeakUsage().getUsed();
        System.out.println("peak old pool shrank: " + (oldLow < oldHigh));
        System.out.println("peak old peak kept after the pool shrank: " + (oldPeak2 >= oldPeak1));
        MemoryUsage edenNow = eden.getUsage();
        MemoryUsage edenPeak = eden.getPeakUsage();
        System.out.println("peak eden peak above its usage after System.gc(): "
                + (edenPeak.getUsed() > edenNow.getUsed()));
        System.out.println("peak eden peak at least 1M: " + (edenPeak.getUsed() >= (1L << 20)));
        System.err.println("[gci-mp14] peak oldLow=" + oldLow + " oldPeak2=" + oldPeak2
                + " eden=" + edenNow + " edenPeak=" + edenPeak);
        for (MemoryPoolMXBean p : pools.values()) {
            MemoryUsage u = p.getUsage();
            MemoryUsage pk = p.getPeakUsage();
            System.out.println("peak " + p.getName() + " peak-covers-usage="
                    + (pk.getUsed() >= u.getUsed() && pk.getCommitted() >= u.getCommitted()));
        }
        old.resetPeakUsage();
        long afterReset = old.getPeakUsage().getUsed();
        long oldNow = old.getUsage().getUsed();
        System.out.println("peak old peak after reset below the old high: " + (afterReset < oldPeak1));
        System.out.println("peak old peak after reset covers the old usage: " + (afterReset >= oldNow));
        System.err.println("[gci-mp14] peak afterReset=" + afterReset + " oldNow=" + oldNow);
    }

    static void info() throws Exception {
        TreeSet<String> heap = new TreeSet<>(heapPools().keySet());
        churn(64L << 20);
        System.gc();
        int ran = 0;
        int notExt = 0;
        boolean ids = true;
        boolean keys = true;
        boolean times = true;
        boolean attr = true;
        boolean nullBefore = true;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (!(gc instanceof com.sun.management.GarbageCollectorMXBean)) {
                notExt++;
                continue;
            }
            com.sun.management.GarbageCollectorMXBean ext = (com.sun.management.GarbageCollectorMXBean) gc;
            long c1 = gc.getCollectionCount();
            GcInfo gi = ext.getLastGcInfo();
            long c2 = gc.getCollectionCount();
            if (c2 <= 0) {
                nullBefore &= gi == null;
                continue;
            }
            if (gi == null) {
                ids = false;
                System.err.println("[gci-mp14] info " + gc.getName() + " count=" + c2 + " null GcInfo");
                continue;
            }
            ran++;
            ids &= gi.getId() >= c1 && gi.getId() <= c2;
            keys &= gi.getMemoryUsageBeforeGc().keySet().containsAll(heap)
                    && gi.getMemoryUsageAfterGc().keySet().containsAll(heap);
            long uptime = ManagementFactory.getRuntimeMXBean().getUptime();
            times &= gi.getStartTime() >= 0 && gi.getEndTime() >= gi.getStartTime()
                    && gi.getDuration() == gi.getEndTime() - gi.getStartTime()
                    && gi.getEndTime() <= uptime;
            attr &= gi.containsKey("GcThreadCount");
            System.err.println("[gci-mp14] info " + gc.getName() + " count=" + c2 + " id=" + gi.getId()
                    + " start=" + gi.getStartTime() + " end=" + gi.getEndTime() + " uptime=" + uptime
                    + " GcThreadCount=" + (gi.containsKey("GcThreadCount") ? gi.get("GcThreadCount") : "-"));
        }
        System.out.println("info some bean ran: " + (ran > 0));
        System.out.println("info every bean is a com.sun.management bean: " + (notExt == 0));
        System.out.println("info no GcInfo before a bean's first collection: " + nullBefore);
        System.out.println("info id within the bean's count: " + ids);
        System.out.println("info every heap pool in both usage maps: " + keys);
        System.out.println("info times ordered: " + times);
        System.out.println("info GcThreadCount attribute: " + attr);
    }

    static void humgrowth() throws Exception {
        MemoryPoolMXBean old = heapPools().get("G1 Old Gen");
        if (old == null) {
            System.out.println("humgrowth not G1");
            return;
        }
        System.gc();
        long base = old.getUsage().getUsed();
        old.setUsageThreshold(base + (8L << 20));
        long t0 = totalCollections();
        long c0 = old.getUsageThresholdCount();
        List<byte[]> hold = new ArrayList<>();
        for (int i = 0; i < 16; i++) {
            hold.add(new byte[1 << 20]);
        }
        Thread.sleep(500);
        long t1 = totalCollections();
        long c1 = old.getUsageThresholdCount();
        System.out.println("humgrowth no collection ran: " + (t1 == t0));
        System.out.println("humgrowth threshold crossed by humongous growth alone: " + (c1 > c0));
        System.err.println("[gci-mp14] humgrowth base=" + base + " now=" + old.getUsage().getUsed()
                + " count=" + c0 + "->" + c1 + " collections=" + t0 + "->" + t1 + " held=" + hold.size());
    }

    static GarbageCollectorMXBean bean(String a, String b) {
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc.getName().equals(a) || gc.getName().equals(b)) {
                return gc;
            }
        }
        return null;
    }

    /**
     * Finding 3: young-only collections leave the old pool's collection usage
     * at HotSpot's initial {@code (init, 0, 0, max)}; a full collection then
     * stamps it with a committed that covers its used.
     */
    static void cu1() throws Exception {
        MemoryPoolMXBean old = find(heapPools(), "Tenured Gen", "G1 Old Gen");
        GarbageCollectorMXBean young = bean("Copy", "G1 Young Generation");
        GarbageCollectorMXBean full = bean("MarkSweepCompact", "G1 Old Generation");
        if (old == null || young == null || full == null) {
            System.out.println("cu1 no beans");
            return;
        }
        long y0 = young.getCollectionCount();
        long f0 = full.getCollectionCount();
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (young.getCollectionCount() < y0 + 3 && System.nanoTime() < deadline) {
            sink = new byte[4096];
        }
        sink = null;
        long y1 = young.getCollectionCount();
        long f1 = full.getCollectionCount();
        System.out.println("cu1 young-only collections ran: " + (y1 >= y0 + 3 && f1 == f0));
        MemoryUsage cu = old.getCollectionUsage();
        System.out.println("cu1 old pool after young collections: used-zero=" + (cu.getUsed() == 0)
                + " committed-zero=" + (cu.getCommitted() == 0));
        System.gc();
        MemoryUsage cu2 = old.getCollectionUsage();
        System.out.println("cu1 old pool after System.gc(): committed-positive=" + (cu2.getCommitted() > 0)
                + " committed-covers-used=" + (cu2.getCommitted() >= cu2.getUsed()));
        System.err.println("[gci-mp14] cu1 young=" + y0 + "->" + y1 + " full=" + f0 + "->" + f1
                + " cu=" + cu + " cu2=" + cu2);
    }

    /** Finding 5 (recorded): G1 eden's committed right after a collection. */
    static void g1commit() {
        System.gc();
        MemoryPoolMXBean eden = heapPools().get("G1 Eden Space");
        if (eden == null) {
            System.out.println("g1commit not G1");
            return;
        }
        MemoryUsage u = eden.getUsage();
        System.out.println("g1commit eden committed above its used after System.gc(): "
                + (u.getCommitted() > u.getUsed()));
        System.err.println("[gci-mp14] g1commit eden=" + u);
    }

    /**
     * Finding 9: the GC notifications of one G1 concurrent cycle (run HotSpot
     * with -XX:+ExplicitGCInvokesConcurrent): HotSpot's concurrent start is a
     * G1 Young Generation pause, G1 Concurrent GC counts Remark and Cleanup.
     */
    static void cycle() throws Exception {
        GarbageCollectorMXBean young = bean("G1 Young Generation", "G1 Young Generation");
        GarbageCollectorMXBean conc = bean("G1 Concurrent GC", "G1 Concurrent GC");
        if (young == null || conc == null) {
            System.out.println("cycle not G1");
            return;
        }
        TreeSet<String> notes = new TreeSet<>();
        int[] concNotes = new int[1];
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info =
                            GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                    synchronized (notes) {
                        notes.add("cycle note " + info.getGcName() + " | " + info.getGcAction() + " | "
                                + info.getGcCause());
                        if (info.getGcName().equals("G1 Concurrent GC")) {
                            concNotes[0]++;
                        }
                    }
                }
            }, null, null);
        }
        long k0 = conc.getCollectionCount();
        long y0 = young.getCollectionCount();
        System.gc();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (conc.getCollectionCount() < k0 + 2 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        Thread.sleep(500);
        long k1 = conc.getCollectionCount();
        long y1 = young.getCollectionCount();
        System.out.println("cycle concurrent collections: " + (k1 - k0));
        synchronized (notes) {
            System.out.println("cycle concurrent notifications: " + concNotes[0]);
            for (String s : notes) {
                System.out.println(s);
            }
        }
        System.err.println("[gci-mp14] cycle conc=" + k0 + "->" + k1 + " young=" + y0 + "->" + y1);
    }
}
