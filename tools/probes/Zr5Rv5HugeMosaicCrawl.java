// zr z5/rv5 probe: does the default near-OOM path (PRESSURE_SELECT=2, overhead
// backstop off) crawl when the free space is fragmented by LIVE huge arrays?
//
// The z4b rows that confirmed the PRESSURE_SELECT=2 flip (grow, mosaic,
// dense, dense16k, large) fragment the LOW region with small objects; the
// tight-heap widening is a low-slide selector change. This shape puts the
// fragmentation at the large-object end instead: a live mosaic of huge arrays
// whose holes are each a little too small for the next huge allocation, while
// a small live set keeps growing (the crawl's "retain ~40 % of every window").
//
// Usage: Zr5Rv5HugeMosaicCrawl <huge_kib> <retain_permille>
//   e.g. `-Xmx256m Zr5Rv5HugeMosaicCrawl 1536 400`
// Prints one line at OOME:
//   oome rounds=<n> huge=<live huge arrays> small=<live small arrays>
//        collections=<sum of GarbageCollectorMXBean counts> cpu_ms=<process CPU>
// Pass: collections and cpu_ms within 2x of `java -XX:+UseZGC` on the same
// host and heap (HotSpot is the twin; rounds/huge/small are informational).
// Positive control: CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=1 (the arm that
// crawled on `Zr2CpCapacity grow`); comparison arm:
// CRATONVM_ZGC_OVERHEAD_BACKSTOP=1.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

public class Zr5Rv5HugeMosaicCrawl {
    public static void main(String[] args) {
        int hugeKib = args.length > 0 ? Integer.parseInt(args[0]) : 1536;
        int retain = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        List<byte[]> huge = new ArrayList<>();
        List<long[]> small = new ArrayList<>();
        long rounds = 0;
        long seed = 0x9E3779B97F4A7C15L;
        try {
            while (true) {
                rounds++;
                // One huge array a little larger than the last freed one.
                int grow = (int) (rounds % 7) * 8 * 1024;
                huge.add(new byte[hugeKib * 1024 + grow]);
                // Free every other huge array now and then: holes too small for
                // the next, larger allocation unless the high end is compacted.
                if (huge.size() > 8 && rounds % 3 == 0) {
                    for (int i = huge.size() - 2; i >= 0; i -= 2) {
                        huge.remove(i);
                        if (huge.size() < 6) {
                            break;
                        }
                    }
                }
                // A window of small allocations, `retain` permille kept live.
                for (int i = 0; i < 256; i++) {
                    seed ^= seed << 13;
                    seed ^= seed >>> 7;
                    seed ^= seed << 17;
                    long[] a = new long[128];
                    if (Math.floorMod(seed, 1000) < retain) {
                        small.add(a);
                    }
                }
            }
        } catch (OutOfMemoryError e) {
            int h = huge.size();
            int s = small.size();
            huge = null;
            small = null;
            long collections = 0;
            for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
                collections += Math.max(0, gc.getCollectionCount());
            }
            long cpuMs = -1;
            java.lang.management.OperatingSystemMXBean os =
                    ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean) {
                cpuMs = ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime()
                        / 1_000_000L;
            }
            System.out.println("oome rounds=" + rounds + " huge=" + h + " small=" + s
                    + " collections=" + collections + " cpu_ms=" + cpuMs);
        }
    }
}
