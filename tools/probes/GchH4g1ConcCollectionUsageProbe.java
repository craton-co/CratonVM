import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;

/**
 * gch h4/g1 -- which G1 pauses update {@code G1 Old Gen}'s collection usage
 * (its collection-usage threshold count), as HotSpot 25 answers it.
 *
 * HotSpot 25 (g1MonitoringSupport.cpp): the incremental manager ({@code G1
 * Young Generation}) lists {@code G1 Old Gen} as NOT always affected, so a
 * young-only pause outside a mixed phase leaves its collection usage alone;
 * the concurrent manager ({@code G1 Concurrent GC}) lists it as always
 * affected, so a Remark / Cleanup pause sets it and runs the collection
 * threshold check. Run HotSpot with {@code -XX:+ExplicitGCInvokesConcurrent} so
 * {@code System.gc()} is a concurrent cycle, as on this VM's G1.
 *
 * Pages: gch-h3os-g1-concurrent-pauses-skip-the-collection-usage-update-and-threshold-check-20261008
 * (the remark half) and gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage-RETIRED-UNFIXED-20261018
 * (the young half).
 *
 * stdout is deterministic booleans; counts on stderr.
 */
public class GchH4g1ConcCollectionUsageProbe {
    static volatile Object sink;
    /** Humongous at -Xmx64m (1 MiB regions): G1 Old Gen is never empty. */
    static byte[] keep = new byte[2 << 20];

    static GarbageCollectorMXBean bean(String name) {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals(name)) {
                return b;
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        MemoryPoolMXBean old = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals("G1 Old Gen")) {
                old = p;
            }
        }
        GarbageCollectorMXBean young = bean("G1 Young Generation");
        GarbageCollectorMXBean conc = bean("G1 Concurrent GC");
        if (old == null || young == null || conc == null) {
            System.out.println("not G1 beans");
            return;
        }
        System.out.println("collection threshold supported: " + old.isCollectionUsageThresholdSupported());
        old.setCollectionUsageThreshold(1);
        long c0 = old.getCollectionUsageThresholdCount();

        // Young-only pauses: short-lived garbage only, no System.gc().
        long y0 = young.getCollectionCount();
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (young.getCollectionCount() < y0 + 3 && System.nanoTime() < deadline) {
            sink = new byte[4096];
        }
        sink = null;
        long y1 = young.getCollectionCount();
        long c1 = old.getCollectionUsageThresholdCount();
        System.out.println("young pauses ran: " + (y1 >= y0 + 3));
        System.out.println("young-only pauses moved the old pool's collection-usage count: " + (c1 != c0));

        // One concurrent cycle.
        long k0 = conc.getCollectionCount();
        System.gc();
        deadline = System.nanoTime() + 10_000_000_000L;
        while (conc.getCollectionCount() < k0 + 2 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        long k1 = conc.getCollectionCount();
        long c2 = old.getCollectionUsageThresholdCount();
        System.out.println("concurrent pauses counted: " + (k1 > k0));
        System.out.println("the concurrent cycle moved the old pool's collection-usage count: " + (c2 != c1));
        System.err.println("[probe] c0=" + c0 + " c1=" + c1 + " c2=" + c2 + " young=" + y0 + "->" + y1
                + " conc=" + k0 + "->" + k1 + " keep=" + keep.length);
        System.out.println("DONE");
    }
}
