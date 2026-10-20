import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.MBeanServer;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.ObjectName;

/**
 * gci i46/hc: is {@code ManagementFactory.getMemoryMXBean()} the bean the platform MBeanServer registers as
 * {@code java.lang:type=Memory}? HotSpot 25 hands out one {@code MemoryImpl}
 * ({@code ManagementFactoryHelper.getMemoryMXBean()}) on every route, so a usage-threshold crossing reaches a
 * listener added through the MBeanServer as well as one added to {@code getMemoryMXBean()}.
 *
 * <p>Run with {@code --add-exports java.management/sun.management=ALL-UNNAMED} on both sides (the helper is read by
 * reflection). stdout = predicates only; figures on stderr ({@code [hc46] ...}).
 */
public class GciI46HcMemoryBeanProbe {
    static final List<Object> KEEP = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        MemoryMXBean mf = ManagementFactory.getMemoryMXBean();
        System.out.println("hc46 same-bean-twice=" + (mf == ManagementFactory.getMemoryMXBean()));
        System.out.println("hc46 same-bean-platform="
                + (mf == ManagementFactory.getPlatformMXBean(MemoryMXBean.class)));
        Object helper = Class.forName("sun.management.ManagementFactoryHelper")
                .getMethod("getMemoryMXBean").invoke(null);
        System.out.println("hc46 same-bean-helper=" + (mf == helper));
        Object reflective = ManagementFactory.class.getMethod("getMemoryMXBean").invoke(null);
        System.out.println("hc46 same-bean-reflective=" + (mf == reflective));

        // Two listeners on what HotSpot treats as one bean: one through the MBeanServer, one on the bean.
        MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        ObjectName memory = new ObjectName(ManagementFactory.MEMORY_MXBEAN_NAME);
        CountDownLatch viaServer = new CountDownLatch(1);
        CountDownLatch viaBean = new CountDownLatch(1);
        AtomicInteger serverCount = new AtomicInteger();
        AtomicInteger beanCount = new AtomicInteger();
        NotificationListener server = (Notification n, Object hb) -> {
            if (n.getType().equals(MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED)) {
                serverCount.incrementAndGet();
                viaServer.countDown();
            }
        };
        NotificationListener bean = (Notification n, Object hb) -> {
            if (n.getType().equals(MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED)) {
                beanCount.incrementAndGet();
                viaBean.countDown();
            }
        };
        mbs.addNotificationListener(memory, server, null, null);
        ((NotificationEmitter) mf).addNotificationListener(bean, null, null);

        MemoryPoolMXBean old = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            String name = p.getName();
            if (p.getType() == MemoryType.HEAP && p.isUsageThresholdSupported()
                    && (name.contains("Old") || name.contains("Tenured"))) {
                old = p;
            }
        }
        if (old == null) {
            System.out.println("hc46 old-pool-found=false");
            return;
        }
        // Put something in the old pool, then arm a 1-byte threshold: the set itself detects the crossing
        // (HotSpot's jmm_SetPoolThreshold -> detect_low_memory); the collection after it is a second chance.
        for (int i = 0; i < 64; i++) {
            KEEP.add(new byte[64 * 1024]);
        }
        System.gc();
        System.err.println("[hc46] old pool " + old.getName() + " used=" + old.getUsage().getUsed());
        old.setUsageThreshold(1);
        System.gc();
        boolean server1 = viaServer.await(10, TimeUnit.SECONDS);
        boolean bean1 = viaBean.await(10, TimeUnit.SECONDS);
        System.err.println("[hc46] notified server=" + serverCount.get() + " bean=" + beanCount.get()
                + " thresholdCount=" + old.getUsageThresholdCount());
        System.out.println("hc46 old-pool-found=true");
        System.out.println("hc46 listener-via-bean-notified=" + bean1);
        System.out.println("hc46 listener-via-mbean-server-notified=" + server1);
        System.out.println("hc46 done");
    }
}
