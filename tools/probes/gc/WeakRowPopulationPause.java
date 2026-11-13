import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

/**
 * P3 item 1: does a young collection pay for the whole weak-row population?
 * For each population K (live WeakReferences to live referents, all promoted by the
 * warm-up), allocate the same garbage and report per-collection pause and count.
 * A flat ms/gc across K means young cycles skip settled rows; growth with K means O(rows).
 * usage: java WeakRowPopulationPause [MB-of-garbage=1024]
 */
public class WeakRowPopulationPause {
    static Object sink;
    static long[] gcStats() {
        long n = 0, ms = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionCount());
            ms += Math.max(0, b.getCollectionTime());
        }
        return new long[] {n, ms};
    }

    static void garbage(long mb) {
        long chunks = mb * 1024 * 1024 / 256;
        for (long i = 0; i < chunks; i++) sink = new byte[200];
    }

    public static void main(String[] a) {
        long mb = a.length > 0 ? Long.parseLong(a[0]) : 1024;
        int[] pops = {0, 20_000, 100_000, 400_000};
        Object[] referents = new Object[400_000];
        WeakReference<?>[] refs = new WeakReference<?>[400_000];
        int have = 0;
        for (int k : pops) {
            for (; have < k; have++) {
                referents[have] = new Object();
                refs[have] = new WeakReference<>(referents[have]);
            }
            garbage(256); // settle: promote the new rows' objects
            long[] s0 = gcStats();
            long t0 = System.nanoTime();
            garbage(mb);
            long wall = (System.nanoTime() - t0) / 1_000_000;
            long[] s1 = gcStats();
            long n = s1[0] - s0[0], ms = s1[1] - s0[1];
            System.out.println("rows=" + k + " gcs=" + n + " gc_ms=" + ms + " ms_per_gc="
                + (n == 0 ? 0 : String.format("%.2f", ms / (double) n)) + " wall_ms=" + wall);
        }
        System.out.println("alive=" + (refs[0].get() != null && refs[have - 1].get() != null));
    }
}
