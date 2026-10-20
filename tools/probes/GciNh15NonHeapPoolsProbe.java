// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i15/nh -- HotSpot 25's NON-heap memory pools and managers
 * ({@code Metaspace}, {@code Compressed Class Space}, the three
 * {@code CodeHeap} pools; {@code CodeCacheManager}, {@code Metaspace Manager})
 * and {@code MemoryMXBean.getNonHeapMemoryUsage()}, as lines a HotSpot 25 run
 * prints identically (Serial by default, G1 with {@code -XX:+UseG1GC}). Page:
 * {@code docs/known-issues/gc/gci-i14mp-memory-pool-and-gc-mxbean-review-findings-20261013.md}
 * item 7.
 *
 * <p>Modes (first argument):
 * <ul>
 * <li>{@code list} -- every pool and manager name (sorted, and in
 *     {@code ManagementFactory}'s own order), each non-heap pool's type,
 *     managers, threshold support and null collection usage, each manager's
 *     pools and whether it is a collector, the collector list.
 * <li>{@code nonheap} -- the aggregate's shape and that it is the non-heap
 *     pools' sum.
 * <li>{@code grow} -- {@code Metaspace} grows when classes load; the code
 *     heaps grow when hot code compiles (bounded wait).
 * <li>{@code peak} -- a non-heap pool's peak covers its usage, before and after
 *     {@code resetPeakUsage()}; a usage threshold of 1 byte is exceeded.
 * <li>{@code gcinfo} -- after {@code System.gc()}, every pool (non-heap too)
 *     is in both {@code GcInfo} usage maps, and {@code Metaspace}'s entry is
 *     positive.
 * <li>{@code mbeans} -- the platform MBean server registers every pool and
 *     manager under its {@code java.lang} name.
 * </ul>
 *
 * <p>stdout is deterministic; sizes, counts and heap-pool details go to stderr
 * ({@code [gci-nh15] ...}).
 */
public final class GciNh15NonHeapPoolsProbe {
    static volatile Object sink;
    static volatile long acc;

    static List<String> sorted(Iterable<String> names) {
        List<String> l = new ArrayList<>();
        for (String s : names) {
            l.add(s);
        }
        l.sort(null);
        return l;
    }

    static TreeMap<String, MemoryPoolMXBean> nonHeapPools() {
        TreeMap<String, MemoryPoolMXBean> m = new TreeMap<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.NON_HEAP) {
                m.put(p.getName(), p);
            }
        }
        return m;
    }

    static long codeHeapUsed() {
        long n = 0;
        for (MemoryPoolMXBean p : nonHeapPools().values()) {
            if (p.getName().startsWith("CodeHeap") || p.getName().equals("CodeCache")) {
                n += p.getUsage().getUsed();
            }
        }
        return n;
    }

    static long metaspaceUsed() {
        MemoryPoolMXBean m = nonHeapPools().get("Metaspace");
        return m == null ? -1 : m.getUsage().getUsed();
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "list";
        System.out.println("mode " + mode);
        if (mode.equals("list")) {
            list();
        } else if (mode.equals("nonheap")) {
            nonheap();
        } else if (mode.equals("grow")) {
            grow();
        } else if (mode.equals("peak")) {
            peak();
        } else if (mode.equals("gcinfo")) {
            gcinfo();
        } else if (mode.equals("mbeans")) {
            mbeans();
        } else {
            System.out.println("unknown mode");
        }
        System.out.println("DONE");
    }

    static void list() {
        List<String> order = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            order.add(p.getName());
        }
        System.out.println("list pools sorted: " + sorted(order));
        System.out.println("list pools in ManagementFactory order: " + order);
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.NON_HEAP) {
                System.err.println("[gci-nh15] heap pool " + p.getName() + " managers="
                        + sorted(Arrays.asList(p.getMemoryManagerNames()))
                        + " usage-threshold=" + p.isUsageThresholdSupported()
                        + " collection-threshold=" + p.isCollectionUsageThresholdSupported());
            }
        }
        for (MemoryPoolMXBean p : nonHeapPools().values()) {
            System.out.println("list non-heap pool " + p.getName()
                    + " valid=" + p.isValid()
                    + " managers=" + sorted(Arrays.asList(p.getMemoryManagerNames()))
                    + " usage-threshold=" + p.isUsageThresholdSupported()
                    + " collection-threshold=" + p.isCollectionUsageThresholdSupported()
                    + " collection-usage-null=" + (p.getCollectionUsage() == null));
            System.err.println("[gci-nh15] " + p.getName() + " usage=" + p.getUsage()
                    + " peak=" + p.getPeakUsage());
        }
        List<String> mgrOrder = new ArrayList<>();
        TreeMap<String, String> mgrLines = new TreeMap<>();
        for (MemoryManagerMXBean m : ManagementFactory.getMemoryManagerMXBeans()) {
            mgrOrder.add(m.getName());
            mgrLines.put(m.getName(), "list manager " + m.getName()
                    + " valid=" + m.isValid()
                    + " collector=" + (m instanceof GarbageCollectorMXBean)
                    + " pools=" + sorted(Arrays.asList(m.getMemoryPoolNames())));
        }
        System.out.println("list managers sorted: " + sorted(mgrOrder));
        System.out.println("list managers in ManagementFactory order: " + mgrOrder);
        for (String line : mgrLines.values()) {
            System.out.println(line);
        }
        List<String> gcs = new ArrayList<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcs.add(gc.getName());
        }
        System.out.println("list collectors sorted: " + sorted(gcs));
    }

    static void nonheap() {
        MemoryUsage a0 = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage();
        long init = 0;
        long used = 0;
        boolean initUndefined = false;
        boolean maxUndefined = false;
        long max = 0;
        for (MemoryPoolMXBean p : nonHeapPools().values()) {
            MemoryUsage u = p.getUsage();
            if (u.getInit() < 0) {
                initUndefined = true;
            } else {
                init += u.getInit();
            }
            if (u.getMax() < 0) {
                maxUndefined = true;
            } else {
                max += u.getMax();
            }
            used += u.getUsed();
        }
        MemoryUsage a1 = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage();
        System.err.println("[gci-nh15] nonheap a0=" + a0 + " a1=" + a1 + " pools-used=" + used
                + " pools-init=" + init + " pools-max=" + (maxUndefined ? -1 : max));
        System.out.println("nonheap init positive: " + (a1.getInit() > 0));
        System.out.println("nonheap used positive: " + (a1.getUsed() > 0));
        System.out.println("nonheap committed covers used: " + (a1.getCommitted() >= a1.getUsed()));
        System.out.println("nonheap max undefined: " + (a1.getMax() == -1));
        System.out.println("nonheap init is the pools' init sum: "
                + (a1.getInit() == (initUndefined ? -1 : init)));
        System.out.println("nonheap max is the pools' max sum: "
                + (a1.getMax() == (maxUndefined ? -1 : max)));
        // Code may be compiled between the three samples: the pools' sum lies
        // within the two aggregates' range, give or take 8 MiB.
        long lo = Math.min(a0.getUsed(), a1.getUsed()) - (8L << 20);
        long hi = Math.max(a0.getUsed(), a1.getUsed()) + (8L << 20);
        System.out.println("nonheap used is the pools' used sum: " + (used >= lo && used <= hi));
    }

    static final String[] CLASSES = {
        "java.util.concurrent.ConcurrentSkipListMap",
        "java.util.concurrent.LinkedTransferQueue",
        "java.util.concurrent.Exchanger",
        "java.util.concurrent.Phaser",
        "java.util.zip.Deflater",
        "java.util.zip.GZIPOutputStream",
        "java.text.SimpleDateFormat",
        "java.text.DecimalFormat",
        "java.text.MessageFormat",
        "java.time.format.DateTimeFormatterBuilder",
        "java.time.chrono.JapaneseDate",
        "java.time.chrono.HijrahDate",
        "java.util.logging.Logger",
        "java.util.logging.XMLFormatter",
        "java.sql.Timestamp",
        "javax.xml.parsers.DocumentBuilderFactory",
        "javax.xml.transform.TransformerFactory",
    };

    static int hot(int x) {
        int r = x;
        for (int i = 0; i < 64; i++) {
            r = r * 31 + (i ^ x);
        }
        return r;
    }

    static long hotLong(long x) {
        long r = x;
        for (int i = 0; i < 64; i++) {
            r = r * 131 + (i | x);
        }
        return r;
    }

    static void grow() throws Exception {
        long meta0 = metaspaceUsed();
        long nh0 = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage().getUsed();
        int loaded = 0;
        for (String c : CLASSES) {
            try {
                sink = Class.forName(c, true, ClassLoader.getSystemClassLoader());
                loaded++;
            } catch (Throwable t) {
                System.err.println("[gci-nh15] grow could not load " + c + ": " + t);
            }
        }
        long meta1 = metaspaceUsed();
        long nh1 = ManagementFactory.getMemoryMXBean().getNonHeapMemoryUsage().getUsed();
        System.err.println("[gci-nh15] grow loaded=" + loaded + " metaspace " + meta0 + "->" + meta1
                + " nonheap " + nh0 + "->" + nh1);
        System.out.println("grow Metaspace grew when classes loaded: " + (meta1 > meta0));
        System.out.println("grow non-heap usage grew when classes loaded: " + (nh1 > nh0));
        long code0 = codeHeapUsed();
        long code1 = code0;
        long deadline = System.nanoTime() + 15_000_000_000L;
        int rounds = 0;
        while (System.nanoTime() < deadline) {
            for (int i = 0; i < 200_000; i++) {
                acc += hot(i) + hotLong(i);
            }
            rounds++;
            code1 = codeHeapUsed();
            if (code1 > code0 && rounds >= 3) {
                break;
            }
        }
        System.err.println("[gci-nh15] grow code " + code0 + "->" + code1 + " rounds=" + rounds);
        System.out.println("grow code heaps grew when hot code ran: " + (code1 > code0));
    }

    static void peak() {
        boolean covers = true;
        boolean coversAfterReset = true;
        for (MemoryPoolMXBean p : nonHeapPools().values()) {
            MemoryUsage u = p.getUsage();
            MemoryUsage pk = p.getPeakUsage();
            covers &= pk.getUsed() >= u.getUsed() && pk.getCommitted() >= u.getCommitted();
            p.resetPeakUsage();
            MemoryUsage u2 = p.getUsage();
            MemoryUsage pk2 = p.getPeakUsage();
            coversAfterReset &= pk2.getUsed() >= u2.getUsed();
            System.err.println("[gci-nh15] peak " + p.getName() + " usage=" + u + " peak=" + pk
                    + " after-reset usage=" + u2 + " peak=" + pk2);
        }
        System.out.println("peak every non-heap peak covers its usage: " + covers);
        System.out.println("peak every non-heap peak covers its usage after a reset: " + coversAfterReset);
        MemoryPoolMXBean meta = nonHeapPools().get("Metaspace");
        if (meta == null || !meta.isUsageThresholdSupported()) {
            System.out.println("peak Metaspace usage threshold unsupported");
            return;
        }
        meta.setUsageThreshold(1);
        System.out.println("peak Metaspace threshold of 1 byte exceeded: " + meta.isUsageThresholdExceeded());
        System.err.println("[gci-nh15] peak Metaspace threshold count=" + meta.getUsageThresholdCount());
        meta.setUsageThreshold(0);
    }

    static void gcinfo() throws Exception {
        Set<String> all = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            all.add(p.getName());
        }
        for (int i = 0; i < 4096; i++) {
            sink = new byte[4096];
        }
        System.gc();
        boolean someInfo = false;
        boolean keys = true;
        boolean metaPositive = true;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (!(gc instanceof com.sun.management.GarbageCollectorMXBean) || gc.getCollectionCount() <= 0) {
                continue;
            }
            GcInfo gi = ((com.sun.management.GarbageCollectorMXBean) gc).getLastGcInfo();
            if (gi == null) {
                continue;
            }
            someInfo = true;
            Map<String, MemoryUsage> before = gi.getMemoryUsageBeforeGc();
            Map<String, MemoryUsage> after = gi.getMemoryUsageAfterGc();
            keys &= before.keySet().containsAll(all) && after.keySet().containsAll(all);
            MemoryUsage m = after.get("Metaspace");
            metaPositive &= m != null && m.getUsed() > 0;
            System.err.println("[gci-nh15] gcinfo " + gc.getName() + " keys=" + sorted(after.keySet())
                    + " Metaspace=" + m);
        }
        System.out.println("gcinfo some GcInfo: " + someInfo);
        System.out.println("gcinfo every pool in both usage maps: " + keys);
        System.out.println("gcinfo Metaspace entry positive: " + metaPositive);
    }

    static void mbeans() throws Exception {
        MBeanServer s = ManagementFactory.getPlatformMBeanServer();
        List<String> pools = new ArrayList<>();
        for (ObjectName n : s.queryNames(new ObjectName("java.lang:type=MemoryPool,*"), null)) {
            pools.add(n.getKeyProperty("name"));
        }
        List<String> mgrs = new ArrayList<>();
        for (ObjectName n : s.queryNames(new ObjectName("java.lang:type=MemoryManager,*"), null)) {
            mgrs.add(n.getKeyProperty("name"));
        }
        System.out.println("mbeans pools: " + sorted(pools));
        System.out.println("mbeans managers: " + sorted(mgrs));
        Object type = s.getAttribute(new ObjectName("java.lang:type=MemoryPool,name=Metaspace"), "Type");
        System.out.println("mbeans Metaspace Type: " + type);
        Object nh = s.getAttribute(new ObjectName("java.lang:type=Memory"), "NonHeapMemoryUsage");
        System.out.println("mbeans NonHeapMemoryUsage present: " + (nh != null));
    }
}
