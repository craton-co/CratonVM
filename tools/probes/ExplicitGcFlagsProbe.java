import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

/** Does an explicit GC request honour `-XX:+DisableExplicitGC` /
 *  `-XX:+ExplicitGCInvokesConcurrent`? (gc-common w8-g)
 *
 *  Makes N explicit requests through each of the three explicit doors
 *  (`System.gc()`, `Runtime.gc()`, `MemoryMXBean.gc()`) and reports how many
 *  collections the GC beans counted and whether a weakly held garbage object
 *  was cleared.
 *
 *  Expected, as on HotSpot:
 *  - no flag:                       collections >= 1, weakCleared=true, verdict FULL
 *  - -XX:+DisableExplicitGC:        collections == 0, weakCleared=false, verdict DISABLED
 *    (unless an allocation-driven collection happens to run; the probe
 *    allocates almost nothing, so that is not expected)
 *  - -XX:+ExplicitGCInvokesConcurrent: the same as no flag (HotSpot G1: a
 *    concurrent-start young pause per request, so collections >= 1 and
 *    weakCleared=true too). */
public class ExplicitGcFlagsProbe {
    static long collections() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) n += c;
        }
        return n;
    }

    public static void main(String[] a) {
        int calls = a.length > 0 ? Integer.parseInt(a[0]) : 10;
        WeakReference<Object> weak = new WeakReference<>(new Object[] {new byte[64]});
        long before = collections();
        for (int i = 0; i < calls; i++) {
            System.gc();
            Runtime.getRuntime().gc();
            ManagementFactory.getMemoryMXBean().gc();
        }
        long after = collections();
        boolean cleared = weak.get() == null;
        long delta = after - before;
        System.out.println("calls=" + (3 * calls) + " collections=" + delta
                + " weakCleared=" + cleared);
        System.out.println(delta == 0 && !cleared ? "DISABLED" : "FULL");
    }
}
