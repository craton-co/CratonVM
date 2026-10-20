// zr z2/g2 (2026-10-12): the deciding workload for element cards
// (`CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`) --
// docs/internal/gc/zgc-zr1gn-object-cards-rescan-whole-old-arrays-SUPERSEDED-20261019.md.
//
// One large OLD reference array (`arrayLen` elements, allocated first and held
// all run) receives `stores` young references per round into scattered slots,
// while each round allocates `perRound` short-lived objects (the nursery
// churn that drives young cycles). With object cards every young cycle
// re-reads the whole array (one card, kept while any element is young); with
// element cards it reads the dirty granules.
//
// Usage: java Zr2G2ArrayCards [arrayLen=1000000] [rounds=300] [perRound=200000] [stores=64]
// Output (stdout, deterministic):
//   zr2g2-arr: len=.. rounds=.. per_round=.. stores=.. checksum=.. ok=..
// stderr: process CPU ms (ThreadMXBean main thread + OperatingSystemMXBean).
import java.lang.management.ManagementFactory;

public class Zr2G2ArrayCards {
    static final class Box {
        final long v;
        Box(long v) { this.v = v; }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int len = arg(args, 0, 1_000_000);
        int rounds = arg(args, 1, 300);
        int perRound = arg(args, 2, 200_000);
        int stores = arg(args, 3, 64);
        Object[] big = new Object[len];
        for (int i = 0; i < len; i += 1024) big[i] = new Box(-i);
        long seed = 0x2545F4914F6CDD1DL;
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < perRound; i++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                Box b = new Box(seed);
                checksum += b.v & 0xF;
                sink = b;
            }
            for (int s = 0; s < stores; s++) {
                int idx = (int) (((long) r * 7919 + (long) s * 104729L) % len);
                big[idx] = new Box((long) r * 1_000L + s);
            }
        }
        boolean ok = true;
        long seen = 0;
        for (int i = 0; i < len; i++) {
            Object o = big[i];
            if (o == null) continue;
            if (!(o instanceof Box)) { ok = false; continue; }
            seen++;
            checksum += ((Box) o).v & 0xFF;
        }
        long cpuMs = -1;
        try {
            java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean) {
                cpuMs = ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime() / 1_000_000;
            }
        } catch (Throwable t) {
            cpuMs = -1;
        }
        System.out.println("zr2g2-arr: len=" + len + " rounds=" + rounds + " per_round=" + perRound
                + " stores=" + stores + " seen=" + seen + " checksum=" + checksum + " ok=" + ok);
        System.err.println("zr2g2-arr-cpu: process_cpu_ms=" + cpuMs);
        if (!ok) System.exit(3);
    }

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }
}
