import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryManagerMXBean;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s3/ob2: the non-heap {@code Metaspace} pool on ZGC, as HotSpot 25 reports it.
 *
 * <p>Stdout carries only shape facts that hold on HotSpot 25 ZGC whatever the
 * figures: the pool's type, managers, threshold support, {@code init}/{@code max},
 * its position before the heap pools, {@code Metaspace Manager} as a plain
 * (non-GC) manager listed before the collectors, the {@code Metaspace} key in
 * every {@code GcInfo} (cycle beans: real figures; pause beans: zeros), and the
 * collector list without the plain manager. Pools HotSpot has and this VM does
 * not model ({@code CodeHeap '...'}, {@code Compressed Class Space}) are named on
 * stderr only.
 *
 * <p>Usage: {@code Zs3Ob2NonHeapPools [maxMetaspaceBytes]} -- pass the
 * {@code -XX:MaxMetaspaceSize} in bytes when the row sets one.
 */
public class Zs3Ob2NonHeapPools {
    static final String MS = "Metaspace";
    static final String MSM = "Metaspace Manager";

    public static void main(String[] args) throws Exception {
        long expectMax = args.length > 0 ? Long.parseLong(args[0]) : -1L;

        List<MemoryPoolMXBean> pools = ManagementFactory.getMemoryPoolMXBeans();
        List<String> poolNames = new ArrayList<>();
        int msIdx = -1, firstHeapIdx = -1;
        MemoryPoolMXBean ms = null;
        for (int i = 0; i < pools.size(); i++) {
            MemoryPoolMXBean p = pools.get(i);
            poolNames.add(p.getName());
            if (p.getName().equals(MS)) {
                msIdx = i;
                ms = p;
            }
            if (p.getType() == MemoryType.HEAP && firstHeapIdx < 0) {
                firstHeapIdx = i;
            }
        }
        System.err.println("[probe] pools " + poolNames);
        if (ms == null) {
            System.out.println("metaspace_pool=absent");
            System.out.println("done");
            return;
        }
        System.out.println("metaspace_pool=present before_heap_pools=" + (msIdx < firstHeapIdx));
        MemoryUsage u = ms.getUsage();
        System.out.println("pool Metaspace type=" + ms.getType()
                + " valid=" + ms.isValid()
                + " managers=" + Arrays.toString(ms.getMemoryManagerNames())
                + " usage_threshold=" + ms.isUsageThresholdSupported()
                + " collection_threshold=" + ms.isCollectionUsageThresholdSupported()
                + " collection_usage=" + (ms.getCollectionUsage() == null ? "null" : "set"));
        System.out.println("usage init=" + u.getInit()
                + " used_pos=" + (u.getUsed() > 0)
                + " committed_ge_used=" + (u.getCommitted() >= u.getUsed())
                + " max_ok=" + (u.getMax() == expectMax));
        MemoryUsage peak = ms.getPeakUsage();
        System.out.println("peak used_ge_0=" + (peak.getUsed() >= 0)
                + " max_ok=" + (peak.getMax() == expectMax));
        try {
            ms.getCollectionUsageThreshold();
            System.out.println("collection_threshold_get=returned");
        } catch (UnsupportedOperationException e) {
            System.out.println("collection_threshold_get=" + e.getClass().getSimpleName() + ": " + e.getMessage());
        }
        try {
            ms.setUsageThreshold(-1);
            System.out.println("usage_threshold_negative=accepted");
        } catch (IllegalArgumentException e) {
            System.out.println("usage_threshold_negative=" + e.getClass().getSimpleName() + ": " + e.getMessage());
        }
        ms.setUsageThreshold(1);
        System.out.println("usage_threshold=1 exceeded=" + ms.isUsageThresholdExceeded());
        ms.setUsageThreshold(0);
        ms.resetPeakUsage();
        System.out.println("reset peak_used_pos=" + (ms.getPeakUsage().getUsed() > 0));
        long nonHeapUsed = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage().getUsed();
        System.out.println("nonheap_used_ge_metaspace_used=" + (nonHeapUsed >= ms.getUsage().getUsed() * 9 / 10));

        // Managers: Metaspace Manager is a plain manager, listed before the collectors.
        List<MemoryManagerMXBean> mgrs = ManagementFactory.getMemoryManagerMXBeans();
        List<String> mgrNames = new ArrayList<>();
        int msmIdx = -1, firstGcIdx = -1;
        for (int i = 0; i < mgrs.size(); i++) {
            MemoryManagerMXBean m = mgrs.get(i);
            mgrNames.add(m.getName());
            if (m.getName().equals(MSM)) {
                msmIdx = i;
                System.out.println("manager Metaspace Manager gc=" + (m instanceof GarbageCollectorMXBean)
                        + " valid=" + m.isValid()
                        + " pools_contain_metaspace=" + Arrays.asList(m.getMemoryPoolNames()).contains(MS)
                        + " object_name=" + m.getObjectName());
            }
            if (m instanceof GarbageCollectorMXBean && firstGcIdx < 0) {
                firstGcIdx = i;
            }
        }
        System.err.println("[probe] managers " + mgrNames);
        System.out.println("manager_before_collectors=" + (msmIdx >= 0 && msmIdx < firstGcIdx));
        List<String> gcs = new ArrayList<>();
        for (GarbageCollectorMXBean g : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcs.add(g.getName());
        }
        System.out.println("collectors " + gcs);
        System.out.println("platform_collectors="
                + ManagementFactory.getPlatformMXBeans(GarbageCollectorMXBean.class).size()
                + " platform_managers_ge_collectors="
                + (ManagementFactory.getPlatformMXBeans(MemoryManagerMXBean.class).size() > gcs.size()));

        // GcInfo: notification + getLastGcInfo after one System.gc().
        CountDownLatch cycle = new CountDownLatch(1);
        List<String> seen = new ArrayList<>();
        for (GarbageCollectorMXBean g : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) g).addNotificationListener((n, h) -> {
                if (!n.getType().equals(GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION)) {
                    return;
                }
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                if (!info.getGcCause().equals("System.gc()")) {
                    return;
                }
                synchronized (seen) {
                    seen.add(info.getGcName() + "|" + shape(info.getGcInfo()));
                }
                if (info.getGcName().endsWith("Cycles")) {
                    cycle.countDown();
                }
            }, null, null);
        }
        System.gc();
        boolean got = cycle.await(10, TimeUnit.SECONDS);
        Thread.sleep(200);
        synchronized (seen) {
            // Distinct shapes: HotSpot sends eight pause notifications per
            // System.gc(), this VM one (zgc-zr1tr pause-count page).
            System.out.println("notified=" + got + " " + new java.util.TreeSet<>(seen));
        }
        for (GarbageCollectorMXBean g : ManagementFactory.getGarbageCollectorMXBeans()) {
            GcInfo gi = ((com.sun.management.GarbageCollectorMXBean) g).getLastGcInfo();
            System.out.println("last " + g.getName() + " " + (gi == null ? "null" : shape(gi)));
        }
        System.out.println("done");
    }

    /** The Metaspace key of a GcInfo, as shape: absent / zero / measured. */
    static String shape(GcInfo gi) {
        MemoryUsage b = gi.getMemoryUsageBeforeGc().get(MS);
        MemoryUsage a = gi.getMemoryUsageAfterGc().get(MS);
        return "metaspace_before=" + edge(b) + " metaspace_after=" + edge(a);
    }

    static String edge(MemoryUsage m) {
        if (m == null) {
            return "absent";
        }
        if (m.getInit() == 0 && m.getUsed() == 0 && m.getCommitted() == 0 && m.getMax() == 0) {
            return "zero";
        }
        return "measured(init=" + m.getInit() + ",used_pos=" + (m.getUsed() > 0)
                + ",committed_ge_used=" + (m.getCommitted() >= m.getUsed())
                + ",max=" + (m.getMax() == -1 ? "-1" : "set") + ")";
    }
}
