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
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zt t1/ob: the non-heap pool set ZGC lists after the zs / dev merge decision
 * ({@code CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS}), against HotSpot 25
 * {@code -XX:+UseZGC}.
 *
 * <p>Stdout carries only facts that hold on HotSpot 25 ZGC whatever the
 * figures: the sorted non-heap pool names with their managers, the plain
 * managers with their sorted pools, each non-heap pool's {@code MemoryUsage}
 * invariants, whether {@code getNonHeapMemoryUsage()} is the sum of the
 * pools (sampled until two pool sums agree, so a JIT install in between does
 * not decide it), and the key set and shape of the {@code GcInfo} of one
 * {@code System.gc()}'s cycle and pause notifications: every pool named, the
 * pause entries all zero, the cycle's {@code Metaspace} and
 * {@code Compressed Class Space} entries real (used above 0, committed at
 * least used). The absolute figures go to stderr.
 */
public class Zt1ObNonHeapPools {
    public static void main(String[] args) throws Exception {
        List<String> nonHeap = new ArrayList<>();
        List<String> allPools = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            allPools.add(p.getName());
            if (p.getType() != MemoryType.NON_HEAP) {
                continue;
            }
            nonHeap.add(p.getName() + "=" + Arrays.toString(p.getMemoryManagerNames()));
            MemoryUsage u = p.getUsage();
            System.err.println("[probe] " + p.getName() + " " + u);
            boolean ok = u.getUsed() >= 0 && u.getCommitted() >= u.getUsed()
                    && (u.getMax() < 0 || u.getMax() >= u.getCommitted());
            System.out.println("pool " + p.getName() + " usage_valid=" + ok
                    + " collection_usage=" + (p.getCollectionUsage() == null ? "null" : "set")
                    + " usage_threshold=" + p.isUsageThresholdSupported()
                    + " collection_threshold=" + p.isCollectionUsageThresholdSupported());
            if (p.getName().equals("Metaspace") || p.getName().equals("Compressed Class Space")) {
                System.out.println("pool " + p.getName() + " used_pos=" + (u.getUsed() > 0)
                        + " init=" + u.getInit());
            }
        }
        Collections.sort(nonHeap);
        System.out.println("non_heap_pools " + nonHeap);

        List<String> plain = new ArrayList<>();
        for (MemoryManagerMXBean m : ManagementFactory.getMemoryManagerMXBeans()) {
            if (m instanceof GarbageCollectorMXBean) {
                continue;
            }
            List<String> ps = new ArrayList<>(Arrays.asList(m.getMemoryPoolNames()));
            Collections.sort(ps);
            plain.add(m.getName() + "=" + ps);
        }
        Collections.sort(plain);
        System.out.println("plain_managers " + plain);

        boolean sumMatches = false;
        for (int i = 0; i < 20 && !sumMatches; i++) {
            long s1 = nonHeapSum();
            MemoryUsage agg = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage();
            long s2 = nonHeapSum();
            System.err.println("[probe] try " + i + " sum1=" + s1 + " aggregate=" + agg.getUsed() + " sum2=" + s2);
            sumMatches = s1 == s2 && agg.getUsed() == s1;
        }
        System.out.println("aggregate_used_is_pool_sum=" + sumMatches);

        // One System.gc(): its cycle and pause GcInfos, as notified.
        List<GcInfo> cycles = Collections.synchronizedList(new ArrayList<>());
        List<GcInfo> pauses = Collections.synchronizedList(new ArrayList<>());
        CountDownLatch seen = new CountDownLatch(1);
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) gc).addNotificationListener((note, hb) -> {
                if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(note.getType())) {
                    return;
                }
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) note.getUserData());
                if (info.getGcName().endsWith("Pauses")) {
                    pauses.add(info.getGcInfo());
                } else if (info.getGcName().endsWith("Cycles")) {
                    cycles.add(info.getGcInfo());
                    seen.countDown();
                }
            }, null, null);
        }
        System.gc();
        boolean notified = seen.await(10, TimeUnit.SECONDS);
        System.out.println("cycle_notified=" + notified);
        TreeSet<String> want = new TreeSet<>(allPools);
        boolean pauseKeys = !pauses.isEmpty();
        boolean pauseZero = !pauses.isEmpty();
        synchronized (pauses) {
            for (GcInfo gi : pauses) {
                for (Map<String, MemoryUsage> m : List.of(gi.getMemoryUsageBeforeGc(), gi.getMemoryUsageAfterGc())) {
                    pauseKeys &= new TreeSet<>(m.keySet()).equals(want);
                    for (MemoryUsage u : m.values()) {
                        pauseZero &= u.getInit() == 0 && u.getUsed() == 0 && u.getCommitted() == 0 && u.getMax() == 0;
                    }
                }
            }
        }
        System.out.println("pause_gcinfo_names_every_pool=" + pauseKeys + " pause_gcinfo_all_zero=" + pauseZero);
        boolean cycleKeys = !cycles.isEmpty();
        boolean msReal = !cycles.isEmpty();
        boolean ccsReal = !cycles.isEmpty();
        synchronized (cycles) {
            for (GcInfo gi : cycles) {
                for (Map<String, MemoryUsage> m : List.of(gi.getMemoryUsageBeforeGc(), gi.getMemoryUsageAfterGc())) {
                    cycleKeys &= new TreeSet<>(m.keySet()).equals(want);
                    MemoryUsage ms = m.get("Metaspace");
                    MemoryUsage ccs = m.get("Compressed Class Space");
                    System.err.println("[probe] cycle " + gi.getId() + " Metaspace " + ms + " CCS " + ccs);
                    msReal &= ms != null && ms.getUsed() > 0 && ms.getCommitted() >= ms.getUsed() && ms.getInit() == 0;
                    ccsReal &= ccs != null && ccs.getUsed() > 0 && ccs.getCommitted() >= ccs.getUsed()
                            && ccs.getInit() == 0 && ccs.getMax() > 0;
                }
            }
        }
        System.out.println("cycle_gcinfo_names_every_pool=" + cycleKeys
                + " cycle_metaspace_real=" + msReal + " cycle_class_space_real=" + ccsReal);
        System.out.println("done");
    }

    static long nonHeapSum() {
        long s = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.NON_HEAP) {
                s += p.getUsage().getUsed();
            }
        }
        return s;
    }
}
