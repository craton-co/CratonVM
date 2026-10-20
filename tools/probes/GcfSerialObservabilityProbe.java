import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gcf f1/r (2026-10-01): the Java-visible GC observability shape of the
 * Generational backend against HotSpot 25 Serial ({@code -XX:+UseSerialGC}).
 *
 * <p>Every stdout line is deterministic (names, booleans, ordering, never a
 * count or a size), so the whole stdout diffs against HotSpot:
 * <ul>
 * <li>{@code collector}: each GarbageCollectorMXBean in platform order, its
 *     pools, and whether it is a NotificationEmitter;</li>
 * <li>{@code pool}: each HEAP pool in platform order, its managers and which
 *     threshold kinds it supports;</li>
 * <li>{@code sysgc}: after one {@code System.gc()}: which beans' counts moved,
 *     the notification's (name, action, cause) and the sorted pool keys of its
 *     {@code GcInfo} after-usage map restricted to heap pools;</li>
 * <li>{@code alloc}: the distinct (name, action, cause) triples allocation
 *     pressure produced (young collections only: the live set stays tiny).</li>
 * </ul>
 * Diagnostics (counts) go to stderr. Run with {@code -Xmx64m}.
 */
public class GcfSerialObservabilityProbe {
    static volatile Object sink;
    static final Map<String, Boolean> TRIPLES = new ConcurrentHashMap<>();
    static volatile String sysgcTriple;
    static volatile String sysgcPools;
    static final CountDownLatch SYSGC_SEEN = new CountDownLatch(1);
    static volatile boolean inSysGc;

    public static void main(String[] args) throws Exception {
        TreeSet<String> heapPoolNames = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            heapPoolNames.add(p.getName());
            System.out.println("pool " + p.getName()
                    + " managers=" + Arrays.toString(p.getMemoryManagerNames())
                    + " usageThreshold=" + p.isUsageThresholdSupported()
                    + " collectionUsageThreshold=" + p.isCollectionUsageThresholdSupported()
                    + " collectionUsage=" + (p.getCollectionUsage() != null));
        }
        NotificationListener listener = (n, h) -> {
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GarbageCollectionNotificationInfo info =
                    GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
            String triple = info.getGcName() + " | " + info.getGcAction() + " | " + info.getGcCause();
            if ("System.gc()".equals(info.getGcCause())) {
                // zr z2/t2: the FIRST System.gc() notification, as the
                // `sysgc notification` line means. It used to be overwritten
                // by every later one, so with several notifications per
                // System.gc() (ZGC: pauses, then the cycle) it printed
                // whichever arrived last before main read it -- a race
                // between delivery speed and main's wake-up.
                if (SYSGC_SEEN.getCount() == 0) {
                    return;
                }
                GcInfo gi = info.getGcInfo();
                TreeSet<String> keys = new TreeSet<>(gi.getMemoryUsageAfterGc().keySet());
                keys.retainAll(heapPoolNames);
                sysgcTriple = triple;
                sysgcPools = keys.toString()
                        + " idPositive=" + (gi.getId() > 0)
                        + " durationNonNegative=" + (gi.getDuration() >= 0)
                        + " endNotBeforeStart=" + (gi.getEndTime() >= gi.getStartTime());
                SYSGC_SEEN.countDown();
            } else {
                TRIPLES.put(triple, Boolean.TRUE);
            }
        };
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();
        for (GarbageCollectorMXBean gc : beans) {
            boolean emitter = gc instanceof NotificationEmitter;
            System.out.println("collector " + gc.getName()
                    + " pools=" + Arrays.toString(gc.getMemoryPoolNames())
                    + " valid=" + gc.isValid()
                    + " emitter=" + emitter);
            if (emitter) {
                ((NotificationEmitter) gc).addNotificationListener(listener, null, null);
            }
        }

        // Allocation pressure: short-lived 8 KiB arrays, a few MiB live at most.
        for (int i = 0; i < 64 * 1024; i++) {
            sink = new byte[8 * 1024];
        }
        Thread.sleep(200);

        long[] before = counts(beans);
        System.gc();
        long[] after = counts(beans);
        StringBuilder moved = new StringBuilder();
        for (int i = 0; i < beans.size(); i++) {
            moved.append(i == 0 ? "" : " ").append(beans.get(i).getName()).append('=')
                    .append(after[i] > before[i] ? "moved" : "still");
            System.err.println("[probe] " + beans.get(i).getName() + " " + before[i] + "->" + after[i]);
        }
        System.out.println("sysgc counts " + moved);
        boolean seen = SYSGC_SEEN.await(5, TimeUnit.SECONDS);
        System.out.println("sysgc notification " + (seen ? sysgcTriple : "NONE"));
        System.out.println("sysgc gcinfo " + (seen ? sysgcPools : "NONE"));

        Thread.sleep(200);
        List<String> triples = new ArrayList<>(new TreeMap<>(TRIPLES).keySet());
        for (String t : triples) {
            System.out.println("alloc notification " + t);
        }
        System.err.println("[probe] alloc triples=" + triples.size());
        System.out.println("done");
    }

    static long[] counts(List<GarbageCollectorMXBean> beans) {
        long[] c = new long[beans.size()];
        for (int i = 0; i < c.length; i++) {
            c[i] = beans.get(i).getCollectionCount();
        }
        return c;
    }
}
