import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.Arrays;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;

/**
 * gc-common w7-f (2026-09-24): the collector and heap-pool beans every backend
 * reports, their counts after real collections, {@code getLastGcInfo()}, and
 * the GC notifications each bean sends. Run on all three backends against
 * HotSpot with the matching flag ({@code -XX:+UseG1GC}, {@code -XX:+UseZGC}
 * [JDK 21: add {@code -XX:-ZGenerational} for the non-generational names this
 * VM reports], {@code -XX:+UseSerialGC} for Generational).
 *
 * <p>Expected on CratonVM:
 * <ul>
 * <li>Generational: {@code Copy}, {@code MarkSweepCompact}; pools
 *     {@code Eden Space}, {@code Survivor Space}, {@code Tenured Gen}.</li>
 * <li>G1: {@code G1 Young Generation} (moves), {@code G1 Old Generation}
 *     (stays 0: this G1 runs no full collection; HotSpot's {@code System.gc()}
 *     is a full one), {@code G1 Concurrent GC} (moves when a marking cycle
 *     ran); pools {@code G1 Eden Space}, {@code G1 Survivor Space},
 *     {@code G1 Old Gen}.</li>
 * <li>ZGC: {@code ZGC Cycles}, {@code ZGC Pauses}; pool {@code ZHeap} —
 *     once {@code handoff-w7f-zgc-backend-gc-beans} is applied; before it,
 *     the historical single {@code G1 Young Generation} bean.</li>
 * </ul>
 * {@code PROBE-OK} = every pool a collector lists exists and lists it back,
 * every heap pool's usage is a valid {@code MemoryUsage}, the summed
 * collection count moved, every bean whose count moved has a
 * {@code getLastGcInfo()}, and at least one notification arrived.
 */
public class GcBeanShapeProbe {
    static final Map<String, AtomicInteger> NOTIFS = new ConcurrentHashMap<>();
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        NotificationListener listener = (n, h) -> {
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo.from(
                    (javax.management.openmbean.CompositeData) n.getUserData());
            String key = info.getGcName() + " | " + info.getGcAction() + " | " + info.getGcCause();
            NOTIFS.computeIfAbsent(key, k -> new AtomicInteger()).incrementAndGet();
        };
        boolean ok = true;
        long before = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            before += Math.max(0, gc.getCollectionCount());
            if (gc instanceof NotificationEmitter) {
                ((NotificationEmitter) gc).addNotificationListener(listener, null, null);
            }
        }
        for (int i = 0; i < 32 * 1024; i++) {
            sink = new byte[8 * 1024];
        }
        System.gc();
        for (int i = 0; i < 16 * 1024; i++) {
            sink = new byte[8 * 1024];
        }
        Thread.sleep(300);

        TreeMap<String, MemoryPoolMXBean> heapPools = new TreeMap<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            heapPools.put(p.getName(), p);
            MemoryUsage u = p.getUsage();
            boolean valid = u.getUsed() >= 0 && u.getUsed() <= u.getCommitted()
                    && (u.getMax() < 0 || u.getCommitted() <= u.getMax());
            ok &= valid;
            System.out.println("pool " + p.getName() + " managers="
                    + Arrays.toString(p.getMemoryManagerNames()) + " used=" + u.getUsed()
                    + " committed=" + u.getCommitted() + " max=" + u.getMax()
                    + " collectionUsed=" + (p.getCollectionUsage() == null ? "null" : p.getCollectionUsage().getUsed())
                    + (valid ? "" : " INVALID"));
        }
        long after = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long count = gc.getCollectionCount();
            after += Math.max(0, count);
            String last = "n/a";
            if (gc instanceof com.sun.management.GarbageCollectorMXBean) {
                com.sun.management.GcInfo gi = ((com.sun.management.GarbageCollectorMXBean) gc).getLastGcInfo();
                last = gi == null ? "null" : ("id=" + gi.getId() + " pools=" + gi.getMemoryUsageAfterGc().keySet());
                if (count > 0 && gi == null) {
                    ok = false;
                }
            }
            for (String pool : gc.getMemoryPoolNames()) {
                MemoryPoolMXBean p = heapPools.get(pool);
                if (p != null && !Arrays.asList(p.getMemoryManagerNames()).contains(gc.getName())) {
                    ok = false;
                    System.out.println("  JOIN-FAIL " + gc.getName() + " -> " + pool);
                }
            }
            System.out.println("collector " + gc.getName() + " count=" + count + " timeMs="
                    + gc.getCollectionTime() + " pools=" + Arrays.toString(gc.getMemoryPoolNames())
                    + " lastGcInfo=" + last);
        }
        ok &= after > before;
        ok &= !NOTIFS.isEmpty();
        for (Map.Entry<String, AtomicInteger> e : new TreeMap<>(NOTIFS).entrySet()) {
            System.out.println("notification " + e.getKey() + " x" + e.getValue().get());
        }
        System.out.println(ok ? "PROBE-OK" : "PROBE-FAIL");
    }
}
