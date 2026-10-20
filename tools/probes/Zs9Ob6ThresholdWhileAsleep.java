import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s9/ob6: a usage threshold set below a pool's usage notifies while the
 * setting thread only sleeps -- no allocation, no collection. HotSpot 25's
 * Service Thread delivers it within milliseconds. Before zs s9/ob6 this VM
 * delivered it only at the setting thread's next allocation door (s6/ob5:
 * the drain hint; zs s6/ob5 `nogc` allocated, so its kill arm still
 * delivered through the 16 KiB occupancy poll).
 *
 * <p>The pool is {@code Metaspace}: HotSpot's {@code ZGC Old Generation}
 * reads 0 used at boot (no threshold of 1 crosses there), this VM's the
 * whole heap, so that pool would compare two different occupancies. Stdout
 * carries shape facts only.
 */
public class Zs9Ob6ThresholdWhileAsleep {
    public static void main(String[] args) throws Exception {
        String want = "Metaspace";
        MemoryPoolMXBean pool = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals(want)) {
                pool = p;
            }
        }
        if (pool == null) {
            System.out.println("pool=absent");
            System.out.println("done");
            return;
        }
        AtomicInteger n = new AtomicInteger();
        StringBuilder seen = new StringBuilder();
        ((NotificationEmitter) ManagementFactory.getMemoryMXBean()).addNotificationListener((no, h) -> {
            MemoryNotificationInfo info = MemoryNotificationInfo.from((CompositeData) no.getUserData());
            synchronized (seen) {
                seen.append(no.getType()).append('|').append(info.getPoolName())
                        .append("|count=").append(info.getCount());
            }
            n.incrementAndGet();
        }, null, null);
        long gcs0 = collections();
        System.err.println("[probe] " + want + " used=" + pool.getUsage().getUsed());
        pool.setUsageThreshold(1);
        // Sleep only: nothing on this thread allocates between the set and
        // the check (the counter and the sleep are allocation-free).
        int i = 0;
        while (i < 300 && n.get() < 1) {
            Thread.sleep(10);
            i++;
        }
        long gcs1 = collections();
        System.err.println("[probe] waited_ms=" + (i * 10));
        System.out.println("asleep pool=" + want + " notified=" + n.get()
                + " count=" + pool.getUsageThresholdCount()
                + " collections=" + (gcs1 - gcs0));
        synchronized (seen) {
            System.out.println("notification " + seen);
        }
        System.out.println("done");
    }

    static long collections() {
        long c = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            c += gc.getCollectionCount();
        }
        return c;
    }
}
