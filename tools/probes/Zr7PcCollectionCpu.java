// zr z7/pc: CPU per whole-heap collection over a retained live set.
//
// Builds a live set of `liveMb` MiB in one of three shapes, then runs `rounds`
// rounds of: replace every 4th retained object (holes across every page, so
// the relocation selector has pages to evacuate), then System.gc(). The
// process CPU time (user+sys, com.sun.management) over the rounds is divided
// by the rounds (one System.gc() each; `gcs` is what the GC MXBeans counted,
// for information only: HotSpot ZGC books cycles AND pauses in its beans).
//
//   prim   int[64] arrays (the Zr2CpCapacity mosaic / grow live set)
//   refs   small objects with two reference fields and a long (a linked graph)
//   mixed  half and half, interleaved
//
// One stdout line per shape:
//   zr7pc shape=<s> live_mb=<n> rounds=<r> gcs=<k> cpu_ms=<t> cpu_ms_per_round=<x> check=<c>
// `check` is a checksum over the live set read back after the last round,
// identical on every JVM (it is what makes the row a correctness row too).
// Compare cpu_ms_per_round against the HotSpot twin and against the kill-switch
// arm of the change under test.
//
// Usage: Zr7PcCollectionCpu <prim|refs|mixed|all> [liveMb=96] [rounds=20]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

public class Zr7PcCollectionCpu {
    static final class Node {
        Node next;
        Object payload;
        long v;
        Node(long v) { this.v = v; }
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

    static Object make(String shape, int i, Object prev) {
        boolean prim = shape.equals("prim") || (shape.equals("mixed") && (i & 1) == 0);
        if (prim) {
            int[] a = new int[64];
            a[0] = i;
            a[63] = i * 7;
            return a;
        }
        Node n = new Node(i);
        n.next = (prev instanceof Node) ? (Node) prev : null;
        n.payload = prev;
        return n;
    }

    static long check(List<Object> live) {
        long sum = 0;
        for (Object o : live) {
            if (o instanceof int[]) {
                int[] a = (int[]) o;
                sum += a[0] + 3L * a[63];
            } else {
                Node n = (Node) o;
                sum += n.v * 5 + (n.next != null ? 1 : 0) + (n.payload != null ? 2 : 0);
            }
        }
        return sum;
    }

    static void run(String shape, int liveMb, int rounds) {
        // ~272 bytes per int[64] and ~48 per Node on HotSpot; the count is the
        // same on every JVM, so the checksum is too.
        int perMb = shape.equals("prim") ? 3800 : shape.equals("refs") ? 21000 : 6500;
        int n = liveMb * perMb;
        List<Object> live = new ArrayList<>(n);
        Object prev = null;
        for (int i = 0; i < n; i++) {
            prev = make(shape, i, prev);
            live.add(prev);
        }
        System.gc();
        long g0 = gcCount();
        long c0 = cpuNanos();
        for (int r = 0; r < rounds; r++) {
            for (int i = r & 3; i < n; i += 4) {
                Object before = i > 0 ? live.get(i - 1) : null;
                live.set(i, make(shape, i, before));
            }
            System.gc();
        }
        long c1 = cpuNanos();
        long g1 = gcCount();

        double cpuMs = (c1 - c0) / 1e6;
        System.out.printf(java.util.Locale.ROOT, "zr7pc shape=%s live_mb=%d rounds=%d gcs=%d cpu_ms=%.0f cpu_ms_per_round=%.1f check=%d%n",
                shape, liveMb, rounds, g1 - g0, cpuMs, cpuMs / rounds, check(live));
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        int liveMb = args.length > 1 ? Integer.parseInt(args[1]) : 96;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        for (String s : new String[] {"prim", "refs", "mixed"}) {
            if (which.equals("all") || which.equals(s)) {
                run(s, liveMb, rounds);
            }
        }
    }
}
