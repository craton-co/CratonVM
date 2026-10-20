import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gcj j1/ob (`g1-and-generational-set-time-threshold-crossing-waits-for-a-drain-door`
 * residual 2, the Generational half): a usage threshold set below a pool's
 * usage notifies while the setting thread only sleeps -- no allocation, no
 * collection after the set. HotSpot 25 Serial's Service Thread delivers it
 * within milliseconds (zs s9/ob6's ZGC row, `Zs9Ob6ThresholdWhileAsleep`, on
 * the Generational backend and its heap pool).
 *
 * <p>Argument: {@code metaspace} (default) or {@code tenured}. For
 * {@code tenured} one {@code System.gc()} BEFORE the set puts the live data
 * in {@code Tenured Gen} (HotSpot: the full collection compacts it there;
 * this VM: `CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON), so a threshold of
 * 1 byte is crossed at the set. Collections are counted from the set on.
 * Stdout carries shape facts only; usage and timing go to stderr.
 */
public class Gcj1ObThresholdWhileAsleep {
    static Object[] keep;

    public static void main(String[] args) throws Exception {
        boolean tenured = args.length > 0 && args[0].equals("tenured");
        String want = tenured ? "Tenured Gen" : "Metaspace";
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
        if (tenured) {
            keep = new Object[64];
            for (int i = 0; i < keep.length; i++) {
                keep[i] = new byte[16 * 1024];
            }
            System.gc();
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
        System.err.println("[gcj1ob] " + want + " used=" + pool.getUsage().getUsed());
        pool.setUsageThreshold(1);
        // Sleep only: nothing on this thread allocates between the set and
        // the check (the counter and the sleep are allocation-free).
        int i = 0;
        while (i < 300 && n.get() < 1) {
            Thread.sleep(10);
            i++;
        }
        long gcs1 = collections();
        System.err.println("[gcj1ob] waited_ms=" + (i * 10));
        System.out.println("asleep pool=" + want + " notified=" + n.get()
                + " count=" + pool.getUsageThresholdCount()
                + " collections=" + (gcs1 - gcs0));
        synchronized (seen) {
            System.out.println("notification " + seen);
        }
        System.out.println("kept=" + keepCount());
        System.out.println("done");
    }

    static int keepCount() {
        return keep == null ? 0 : keep.length;
    }

    static long collections() {
        long c = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            c += gc.getCollectionCount();
        }
        return c;
    }
}
