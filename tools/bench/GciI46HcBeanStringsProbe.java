import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i46/hc: two HotSpot 25 answers of the GC beans that do not depend on timing.
 *
 * <ul>
 * <li>{@code ManagementFactory.getGarbageCollectorMXBeans()} order: the JDK lists the beans from a
 *     {@code HashMap} keyed by their canonical object names (G1: Young, Concurrent, Old).
 * <li>The strings HotSpot creates fresh ({@code create_from_str}): a bean's name, a GC notification's name, action
 *     and cause. A string is "interned" here when an equal string interned FIRST from a separate copy is the same
 *     object (a fresh string never is; asking {@code s == s.intern()} would intern {@code s} itself).
 * </ul>
 * stdout = predicates only; names on stderr ({@code [hc46] ...}).
 */
public class GciI46HcBeanStringsProbe {
    static boolean interned(String s) {
        return s == new String(s.toCharArray()).intern();
    }

    public static void main(String[] args) throws Exception {
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        List<String> names = new ArrayList<>();
        for (GarbageCollectorMXBean gc : gcs) {
            names.add(gc.getName());
        }
        System.out.println("hc46 gc-list=" + String.join(",", names));
        boolean anyGcInterned = false;
        for (GarbageCollectorMXBean gc : gcs) {
            anyGcInterned |= interned(gc.getName());
        }
        System.out.println("hc46 collector-names-interned=" + anyGcInterned);
        boolean anyPoolInterned = false;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (interned(p.getName())) {
                System.err.println("[hc46] interned pool name " + p.getName());
                anyPoolInterned = true;
            }
        }
        System.out.println("hc46 pool-names-interned=" + anyPoolInterned);

        CountDownLatch got = new CountDownLatch(1);
        AtomicReference<GarbageCollectionNotificationInfo> first = new AtomicReference<>();
        NotificationListener l = (n, hb) -> {
            if (n.getType().equals(GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION)
                    && first.compareAndSet(null,
                            GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData()))) {
                got.countDown();
            }
        };
        for (GarbageCollectorMXBean gc : gcs) {
            ((NotificationEmitter) gc).addNotificationListener(l, null, null);
        }
        System.gc();
        boolean notified = got.await(10, TimeUnit.SECONDS);
        System.out.println("hc46 notified=" + notified);
        if (notified) {
            GarbageCollectionNotificationInfo info = first.get();
            System.err.println("[hc46] first notification " + info.getGcName() + " | " + info.getGcAction()
                    + " | " + info.getGcCause());
            System.out.println("hc46 notification-strings-interned="
                    + (interned(info.getGcName()) || interned(info.getGcAction()) || interned(info.getGcCause())));
        }
        System.out.println("hc46 done");
    }
}
