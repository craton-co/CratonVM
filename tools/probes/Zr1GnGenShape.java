// zr z1/gn (2026-10-12): a GENERATIONAL-SHAPED workload for ZGC's
// generational mode -- the shape
// docs/internal/zgc-round-20260920/measurement-nursery-survivor-census-20260921.md
// says was never run: most objects die before their first young cycle, a
// medium old set lives for the whole run, and a small fraction of each round
// survives a few cycles (clustered: one round's survivors are allocated
// together) and is then dropped.
//
// Usage: java Zr1GnGenShape <oldMiB> <rounds> <perRound> <survivePermille> <holdRounds> [storesPerRound]
//   oldMiB          medium old set, retained for the whole run (default 64)
//   rounds          rounds of churn (default 400)
//   perRound        short-lived objects per round (default 200000)
//   survivePermille of those, how many per thousand are kept (default 5)
//   holdRounds      how many rounds a kept batch is held before release (default 8)
//   storesPerRound  old->young stores per round into the old set (default 64)
//
// Output (stdout), one line each, stable for diffing:
//   zr1gn-rows: old_mib=.. rounds=.. per_round=.. survive_permille=.. hold=.. stores=..
//   zr1gn-cpu: cpu_ms=<process CPU, ThreadMXBean sum of live threads + main> wall_ms=..
//   zr1gn-gc: <name>=count/ms ...   (GarbageCollectorMXBean totals)
//   zr1gn-check: checksum=<deterministic> ok=<true|false>
// The checksum depends only on the arguments, so it must be identical on
// every collector and every switch setting -- the pass criterion for
// correctness. CPU and GC totals are the performance rows.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;

public class Zr1GnGenShape {
    static final class Node {
        Node next;
        Object payload;
        long value;
        Node(long v) { value = v; }
    }

    static final class Leaf {
        final long a;
        final int[] small;
        Leaf(long a) { this.a = a; this.small = new int[4]; small[0] = (int) a; }
    }

    public static void main(String[] args) {
        int oldMiB = arg(args, 0, 64);
        int rounds = arg(args, 1, 400);
        int perRound = arg(args, 2, 200_000);
        int survivePermille = arg(args, 3, 5);
        int hold = arg(args, 4, 8);
        int stores = arg(args, 5, 64);
        System.out.println("zr1gn-rows: old_mib=" + oldMiB + " rounds=" + rounds + " per_round=" + perRound
                + " survive_permille=" + survivePermille + " hold=" + hold + " stores=" + stores);

        ThreadMXBean tmx = ManagementFactory.getThreadMXBean();
        long cpu0 = tmx.isCurrentThreadCpuTimeSupported() ? tmx.getCurrentThreadCpuTime() : 0L;
        long wall0 = System.nanoTime();

        // The medium old set: ~48 bytes per node plus a 64-byte payload array,
        // so ~oldMiB MiB in a few hundred thousand objects. Lives all run.
        int oldCount = Math.max(1, (oldMiB * 1024 * 1024) / 128);
        Node[] old = new Node[oldCount];
        for (int i = 0; i < oldCount; i++) {
            Node n = new Node(i);
            n.payload = new long[6];
            if (i > 0) old[i - 1].next = n;
            old[i] = n;
        }

        // Batches kept for `hold` rounds, then released: the clustered,
        // medium-lived survivors.
        List<Object[]> held = new ArrayList<>();
        long checksum = 0;
        long seed = 0x9E3779B97F4A7C15L;
        for (int r = 0; r < rounds; r++) {
            int keep = (int) ((long) perRound * survivePermille / 1000);
            Object[] batch = new Object[Math.max(keep, 1)];
            int kept = 0;
            for (int i = 0; i < perRound; i++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                Object o = ((seed >>> 33) & 3) == 0 ? new Leaf(seed) : new Node(seed);
                // The first `keep` of each round survive: allocated together,
                // so a batch shares nursery pages.
                if (kept < keep) {
                    batch[kept++] = o;
                }
                checksum += (o instanceof Leaf) ? ((Leaf) o).small[0] : ((Node) o).value & 0xFF;
            }
            held.add(batch);
            if (held.size() > hold) {
                Object[] gone = held.remove(0);
                checksum += gone.length;
            }
            // Old -> young stores: the remembered set's work. Deterministic slots.
            for (int s = 0; s < stores; s++) {
                int idx = (int) (((long) r * 7919 + s * 104729L) % oldCount);
                old[idx].payload = (s & 1) == 0 ? new Leaf(r + s) : batch[s % batch.length];
            }
        }

        // Read everything back: the old chain must be intact and every held
        // batch walkable. A freed-and-zeroed object reads as null / 0 here.
        boolean ok = true;
        long chain = 0;
        for (Node n = old[0]; n != null; n = n.next) {
            chain++;
            if (n.payload == null) { ok = false; }
            else if (n.payload instanceof Leaf) checksum += ((Leaf) n.payload).a & 0xF;
        }
        if (chain != oldCount) ok = false;
        for (Object[] b : held) {
            for (Object o : b) {
                if (o == null) { ok = false; continue; }
                checksum += (o instanceof Leaf) ? ((Leaf) o).small[0] & 0xF : ((Node) o).value & 0xF;
            }
        }

        long cpuNs = tmx.isCurrentThreadCpuTimeSupported() ? tmx.getCurrentThreadCpuTime() - cpu0 : -1;
        long wallMs = (System.nanoTime() - wall0) / 1_000_000;
        // Whole-process CPU where available (GC threads included).
        long procCpuMs = -1;
        try {
            java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean) {
                procCpuMs = ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime() / 1_000_000;
            }
        } catch (Throwable t) {
            procCpuMs = -1;
        }
        System.out.println("zr1gn-cpu: main_cpu_ms=" + (cpuNs < 0 ? -1 : cpuNs / 1_000_000)
                + " process_cpu_ms=" + procCpuMs + " wall_ms=" + wallMs);
        StringBuilder gc = new StringBuilder("zr1gn-gc:");
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            gc.append(' ').append(b.getName().replace(' ', '_')).append('=')
              .append(b.getCollectionCount()).append('/').append(b.getCollectionTime()).append("ms");
        }
        System.out.println(gc);
        System.out.println("zr1gn-check: checksum=" + checksum + " ok=" + ok);
        if (!ok) System.exit(3);
    }

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }
}
