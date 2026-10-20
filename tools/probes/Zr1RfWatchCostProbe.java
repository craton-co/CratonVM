import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.ref.WeakReference;

/**
 * zr z1/rf (2026-10-12): the per-collection cost of the reference passes when a
 * program holds many live {@code WeakReference}s (the {@code ThreadLocalMap$Entry} /
 * {@code WeakHashMap} shape). CratonVM row: {@code -XX:+UseZGC -Xmx512m}, default
 * against {@code CRATONVM_ZGC_SKIP_WATCHED_SET=0} (the arm that builds the
 * watched-referent set every collection), interleaved pairs.
 *
 * <p>Arguments: {@code [refs] [gcs]} (default 200,000 and 50). Every reference
 * stays reachable and set, so stdout is deterministic: the count still set at the
 * end, which must equal {@code refs} on HotSpot and CratonVM alike. stderr: the
 * CPU milliseconds of the {@code System.gc()} loop on this thread (ThreadMXBean),
 * the figure to compare between the two CratonVM arms (a CratonVM collection runs
 * on the thread that asked for it, so the reference passes are in it; HotSpot's
 * collector threads are not, so the HotSpot twin is a stdout twin only). Read
 * {@code /usr/bin/time} user+sys beside it.
 */
public class Zr1RfWatchCostProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int gcs = args.length > 1 ? Integer.parseInt(args[1]) : 50;
        Object[] referents = new Object[n];
        @SuppressWarnings("unchecked")
        WeakReference<Object>[] refs = new WeakReference[n];
        for (int i = 0; i < n; i++) {
            referents[i] = new Object();
            refs[i] = new WeakReference<>(referents[i]);
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long cpu0 = mx.getCurrentThreadCpuTime();
        for (int g = 0; g < gcs; g++) {
            System.gc();
        }
        long cpuMs = (mx.getCurrentThreadCpuTime() - cpu0) / 1_000_000L;
        int set = 0;
        for (int i = 0; i < n; i++) {
            if (refs[i].refersTo(referents[i])) {
                set++;
            }
        }
        System.err.println("gc-loop-cpu-ms=" + cpuMs + " gcs=" + gcs + " refs=" + n);
        System.out.println("still set: " + set + " of " + n);
        System.out.println("done");
    }
}
