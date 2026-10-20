import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s6/ob5: the ZGC {@code Metaspace} pool at the edges of a collection, as
 * HotSpot 25 reports it. One mode per run (each needs a fresh VM):
 *
 * <ul>
 *   <li>{@code first}: a listener on {@code ZGC Major Cycles} is installed
 *       WITHOUT enumerating the memory pools, then {@code System.gc()}; the
 *       first cycle notification's {@code GcInfo} and the bean's
 *       {@code getLastGcInfo()} must carry a measured {@code Metaspace} key
 *       (HotSpot always does; before zs s6/ob5 this VM read the JMM's
 *       undefined usage, {@code init = max = -1}, for every collection that
 *       ran before the pool was first handed out).
 *   <li>{@code last}: the same through {@code getLastGcInfo()} only (no
 *       listener: the pool is handed out by the {@code GcInfoBuilder}).
 *   <li>{@code threshold}: a {@code Metaspace} usage threshold fires a
 *       {@code MEMORY_THRESHOLD_EXCEEDED} notification and counts, clears
 *       when raised, and fires again when lowered.
 *   <li>{@code peak}: {@code getPeakUsage()} keeps the class metadata a
 *       collection saw after the classes are unloaded.
 * </ul>
 *
 * <p>Stdout carries shape facts only; figures go to stderr.
 */
public class Zs6Ob5MetaspaceEdges {
    static final String MS = "Metaspace";
    static final String CYCLES = "ZGC Major Cycles";

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "first";
        switch (mode) {
            case "first" -> first(true);
            case "last" -> first(false);
            case "threshold" -> threshold();
            case "peak" -> peak();
            case "nogc" -> nogc();
            default -> throw new IllegalArgumentException(mode);
        }
        System.out.println("done");
    }

    static String shape(MemoryUsage u) {
        if (u == null) {
            return "absent";
        }
        if (u.getInit() == -1 && u.getMax() == -1 && u.getUsed() == 0) {
            return "undefined";
        }
        if (u.getInit() == 0 && u.getUsed() == 0 && u.getCommitted() == 0 && u.getMax() == 0) {
            return "zero";
        }
        return "measured(init=" + u.getInit()
                + ",used_pos=" + (u.getUsed() > 0)
                + ",committed_ge_used=" + (u.getCommitted() >= u.getUsed())
                + ",max=" + (u.getMax() == -1 ? "-1" : "set") + ")";
    }

    static String metaspace(GcInfo gi) {
        if (gi == null) {
            return "gcinfo=null";
        }
        MemoryUsage b = gi.getMemoryUsageBeforeGc().get(MS);
        MemoryUsage a = gi.getMemoryUsageAfterGc().get(MS);
        System.err.println("[probe] id=" + gi.getId() + " metaspace before=" + b + " after=" + a);
        return "metaspace_before=" + shape(b) + " metaspace_after=" + shape(a);
    }

    static GarbageCollectorMXBean bean(String name) {
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc.getName().equals(name)) {
                return gc;
            }
        }
        throw new IllegalStateException("no bean " + name);
    }

    static void first(boolean listen) throws Exception {
        GarbageCollectorMXBean cycles = bean(CYCLES);
        AtomicReference<GcInfo> seen = new AtomicReference<>();
        if (listen) {
            ((NotificationEmitter) cycles).addNotificationListener((n, h) -> {
                if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    return;
                }
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                seen.compareAndSet(null, info.getGcInfo());
            }, null, null);
        }
        System.gc();
        if (listen) {
            for (int i = 0; i < 500 && seen.get() == null; i++) {
                Thread.sleep(10);
            }
            System.out.println("first_notification " + metaspace(seen.get()));
        }
        GcInfo last = ((com.sun.management.GarbageCollectorMXBean) cycles).getLastGcInfo();
        System.out.println("last_gcinfo " + metaspace(last));
    }

    static MemoryPoolMXBean metaspacePool() {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals(MS)) {
                return p;
            }
        }
        return null;
    }

    static void await(AtomicInteger n, int want) throws InterruptedException {
        for (int i = 0; i < 300 && n.get() < want; i++) {
            Thread.sleep(10);
        }
    }

    static void threshold() throws Exception {
        MemoryPoolMXBean ms = metaspacePool();
        if (ms == null) {
            System.out.println("metaspace_pool=absent");
            return;
        }
        AtomicInteger n = new AtomicInteger();
        List<String> kinds = new ArrayList<>();
        ((NotificationEmitter) ManagementFactory.getMemoryMXBean()).addNotificationListener((no, h) -> {
            MemoryNotificationInfo info = MemoryNotificationInfo.from((CompositeData) no.getUserData());
            synchronized (kinds) {
                kinds.add(no.getType() + "|" + info.getPoolName() + "|count=" + info.getCount());
            }
            n.incrementAndGet();
        }, null, null);
        ms.setUsageThreshold(1);
        System.gc();
        await(n, 1);
        Thread.sleep(200);
        System.out.println("low notified=" + n.get() + " count=" + ms.getUsageThresholdCount()
                + " exceeded=" + ms.isUsageThresholdExceeded());
        ms.setUsageThreshold(1L << 40);
        System.gc();
        Thread.sleep(200);
        System.out.println("high notified=" + n.get() + " count=" + ms.getUsageThresholdCount()
                + " exceeded=" + ms.isUsageThresholdExceeded());
        ms.setUsageThreshold(1);
        System.gc();
        await(n, 2);
        Thread.sleep(200);
        System.out.println("relow notified=" + n.get() + " count=" + ms.getUsageThresholdCount()
                + " exceeded=" + ms.isUsageThresholdExceeded());
        synchronized (kinds) {
            System.out.println("notifications " + kinds);
        }
    }

    static volatile Object sink;

    /**
     * A threshold set below the usage notifies with no collection: HotSpot's
     * Service Thread delivers it. The loop allocates a little (an allocation
     * door is this VM's drain point) and never enough to collect at -Xmx256m.
     */
    static void nogc() throws Exception {
        MemoryPoolMXBean ms = metaspacePool();
        if (ms == null) {
            System.out.println("metaspace_pool=absent");
            return;
        }
        AtomicInteger n = new AtomicInteger();
        ((NotificationEmitter) ManagementFactory.getMemoryMXBean())
                .addNotificationListener((no, h) -> n.incrementAndGet(), null, null);
        long gcs0 = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcs0 += gc.getCollectionCount();
        }
        ms.setUsageThreshold(1);
        for (int i = 0; i < 300 && n.get() < 1; i++) {
            sink = new byte[64];
            Thread.sleep(10);
        }
        long gcs1 = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcs1 += gc.getCollectionCount();
        }
        System.out.println("nogc notified=" + n.get() + " count=" + ms.getUsageThresholdCount()
                + " collections=" + (gcs1 - gcs0));
    }

    /** Loaded once per throwaway loader: some metadata to unload. */
    public static class Payload {
        public int a(int x) { return x + 1; }
        public int b(int x) { return x * 2; }
        public int c(int x) { return x - 3; }
        public String d(String s) { return s + "d"; }
        public String e(String s) { return s.trim(); }
        public long f(long v) { return v ^ 0x5DEECE66DL; }
        public double g(double v) { return Math.sqrt(v); }
        public Object h(Object o) { return o == null ? "null" : o.toString(); }
    }

    static void peak() throws Exception {
        MemoryPoolMXBean ms = metaspacePool();
        if (ms == null) {
            System.out.println("metaspace_pool=absent");
            return;
        }
        URL here = Zs6Ob5MetaspaceEdges.class.getProtectionDomain().getCodeSource().getLocation();
        ms.resetPeakUsage();
        List<URLClassLoader> loaders = new ArrayList<>();
        List<Class<?>> classes = new ArrayList<>();
        for (int i = 0; i < 200; i++) {
            URLClassLoader l = new URLClassLoader(new URL[] {here}, null);
            Class<?> c = l.loadClass("Zs6Ob5MetaspaceEdges$Payload");
            c.getDeclaredMethods();
            loaders.add(l);
            classes.add(c);
        }
        // The collection sees the payload classes alive.
        System.gc();
        long high = ms.getUsage().getUsed();
        boolean distinct = classes.get(0) != classes.get(1);
        loaders.clear();
        classes.clear();
        System.gc();
        System.gc();
        long low = ms.getUsage().getUsed();
        MemoryUsage peak = ms.getPeakUsage();
        System.err.println("[probe] high=" + high + " low=" + low + " peak=" + peak);
        System.out.println("distinct_classes=" + distinct);
        System.out.println("unloaded=" + (low < high));
        System.out.println("peak_ge_high=" + (peak.getUsed() >= high)
                + " peak_committed_ge_used=" + (peak.getCommitted() >= peak.getUsed()));
    }
}
