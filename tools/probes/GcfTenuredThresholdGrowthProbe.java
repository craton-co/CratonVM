import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gcf f4/r (2026-10-01): the old generation's usage threshold must fire when
 * the old generation GROWS between collections, as HotSpot Serial's
 * low-memory detector does after a slow-path allocation
 * (gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth).
 *
 * <p>Sets the old pool's usage threshold to its usage + 8 MiB, allocates ONE
 * array of {@code mib} MiB (default 24; with {@code -Xmx64m} it does not fit
 * the young generation, so it is allocated in the old generation directly),
 * then waits up to 1 s WITHOUT allocating and without {@code System.gc()}.
 *
 * <p>Stdout (HotSpot 25.0.3 Serial, {@code -Xmx64m}): {@code above=true} then
 * {@code threshold-exceeded Tenured Gen count=1}. A VM that checks the
 * threshold only at collections prints {@code no-notification} instead (the
 * negative control).
 */
public class GcfTenuredThresholdGrowthProbe {
    static Object keep;

    public static void main(String[] args) throws Exception {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 24;
        MemoryPoolMXBean old = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP && p.isUsageThresholdSupported()) {
                old = p;
            }
        }
        if (old == null) {
            System.out.println("no-old-pool");
            return;
        }
        CountDownLatch seen = new CountDownLatch(1);
        String[] got = new String[1];
        NotificationEmitter emitter = (NotificationEmitter) ManagementFactory.getMemoryMXBean();
        emitter.addNotificationListener((n, hb) -> {
            if (MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED.equals(n.getType())) {
                MemoryNotificationInfo info = MemoryNotificationInfo.from((CompositeData) n.getUserData());
                got[0] = "threshold-exceeded " + info.getPoolName() + " count=" + info.getCount();
                seen.countDown();
            }
        }, null, null);
        long threshold = old.getUsage().getUsed() + 8L * 1024 * 1024;
        old.setUsageThreshold(threshold);
        keep = new byte[mib * 1024 * 1024];
        System.out.println("above=" + (old.getUsage().getUsed() >= threshold));
        boolean ok = seen.await(1, TimeUnit.SECONDS);
        System.out.println(ok ? got[0] : "no-notification");
    }
}
