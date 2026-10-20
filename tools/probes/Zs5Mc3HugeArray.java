// zs s5/mc3: one HUGE reference array of live objects as the mark's critical path, the shape
// the stealable array chunks (CRATONVM_ZGC_MARK_SPLIT_ARRAYS) exist for.
//
//   huge    ONE Object[n] holding n live Small objects. Before the split one mark worker
//           walked all n slots (read, registry test, claim) while the others waited for its
//           published leaves; with it the array's chunks spread over the pool.
//   quad    four Object[n/4] (several splits at once; the stealing must not starve one)
//   hubs    n/64 Object[64] hubs: no array longer than one chunk (4096), so NOTHING splits.
//           The negative control: array_splits=0 on every mark line, CPU unchanged.
//
// Every round replaces one element in four with a fresh Small (old ones become garbage) and
// calls System.gc().
//
// STDOUT is deterministic (identical on every JVM):
//   zs5mc3 shape=<s> n=<n> rounds=<r> check=<c>
// STDERR carries timing, never compared:
//   zs5mc3-cpu shape=<s> cpu_ms=<t> cpu_ms_per_round=<x> gcs=<g>
//
// Usage: Zs5Mc3HugeArray <huge|quad|hubs|all> [n=8000000] [rounds=20]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zs5Mc3HugeArray {
    static final class Small {
        final long v;
        Small(long v) { this.v = v; }
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) n += c;
        }
        return n;
    }

    static long cpuNanos() {
        java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        if (os instanceof com.sun.management.OperatingSystemMXBean) {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
        }
        return -1;
    }

    static void run(String shape, int n, int rounds) {
        Object[][] arrays;
        if (shape.equals("huge")) {
            arrays = new Object[][] {new Object[n]};
        } else if (shape.equals("quad")) {
            arrays = new Object[4][];
            for (int a = 0; a < 4; a++) arrays[a] = new Object[n / 4];
        } else {
            arrays = new Object[n / 64][];
            for (int a = 0; a < arrays.length; a++) arrays[a] = new Object[64];
        }
        long k = 0;
        for (Object[] arr : arrays) {
            for (int i = 0; i < arr.length; i++) arr[i] = new Small(k++);
        }
        System.gc();
        long g0 = gcCount();
        long c0 = cpuNanos();
        for (int r = 0; r < rounds; r++) {
            for (Object[] arr : arrays) {
                for (int i = r & 3; i < arr.length; i += 4) {
                    arr[i] = new Small(((Small) arr[i]).v + 1);
                }
            }
            System.gc();
        }
        long c1 = cpuNanos();
        long g1 = gcCount();
        long check = 0;
        for (Object[] arr : arrays) {
            for (Object o : arr) check += ((Small) o).v;
        }
        System.out.printf(java.util.Locale.ROOT,
                "zs5mc3 shape=%s n=%d rounds=%d check=%d%n", shape, k, rounds, check);
        double cpuMs = (c1 - c0) / 1e6;
        System.err.printf(java.util.Locale.ROOT,
                "zs5mc3-cpu shape=%s cpu_ms=%.0f cpu_ms_per_round=%.1f gcs=%d%n",
                shape, cpuMs, cpuMs / rounds, g1 - g0);
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 8_000_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        for (String s : new String[] {"huge", "quad", "hubs"}) {
            if (which.equals("all") || which.equals(s)) {
                run(s, n, rounds);
            }
        }
    }
}
