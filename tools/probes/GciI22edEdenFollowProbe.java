import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i22/ed ({@code gch-h4fl-proposal-serial-eden-follows-the-initial-heap-20261009},
 * {@code CRATONVM_GEN_SERIAL_EDEN_INITIAL}): young-collection counts per phase, to
 * compare against HotSpot Serial, whose young generation starts at the INITIAL
 * heap's share ({@code InitialHeapSize / 3}) and is resized after a full
 * collection ({@code old capacity / NewRatio}).
 *
 * <p>Phases: a 256 MB warm-up churn (not reported); {@code start}: {@code churnMb}
 * MB of 512-byte arrays, a 256-slot ring keeping a few alive; {@code grow}:
 * {@code retainMb} MB of 64 KiB arrays kept alive, then one {@code System.gc()}
 * (HotSpot grows the old generation to about {@code retained / 0.6} and the young
 * one to half of that); {@code after}: the {@code start} churn again.
 *
 * <p>stdout is deterministic (one checksum line per phase, the retained sum, and
 * {@code EDEN-FOLLOW-OK}); stderr carries {@code [ed22] phase=P young=N full=N ms=T},
 * the collector MXBeans' count deltas over the phase ({@code Copy} is the young
 * collector on HotSpot Serial and on the Generational backend).
 *
 * <p>Usage: {@code GciI22edEdenFollowProbe [churnMb] [retainMb]} (defaults 6000, 600).
 */
public final class GciI22edEdenFollowProbe {
    static long sink;

    static boolean isYoung(String name) {
        return name.equals("Copy") || name.contains("Young") || name.equals("PS Scavenge")
                || name.equals("ParNew");
    }

    static long count(boolean young) {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (isYoung(b.getName()) == young) {
                n += Math.max(0L, b.getCollectionCount());
            }
        }
        return n;
    }

    /** {@code mb} MB of 512-byte {@code long[62]} arrays; a 256-slot ring keeps a few alive. */
    static long churn(long mb, int seed) {
        long[][] ring = new long[256][];
        long check = seed;
        long n = mb * 2048L;
        for (long i = 0; i < n; i++) {
            long[] a = new long[62];
            a[0] = i ^ seed;
            a[61] = i;
            int slot = (int) (i & 255L);
            long[] prev = ring[slot];
            if (prev != null) {
                check += prev[0] + prev[61];
            }
            ring[slot] = a;
        }
        return check;
    }

    static List<long[]> retain(int mb) {
        List<long[]> keep = new ArrayList<>();
        for (int i = 0; i < mb * 16; i++) {
            long[] a = new long[8190];
            a[0] = i;
            a[8189] = i;
            keep.add(a);
        }
        return keep;
    }

    static void report(String phase, long y0, long f0, long t0) {
        long y1 = count(true);
        long f1 = count(false);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.err.println("[ed22] phase=" + phase + " young=" + (y1 - y0) + " full=" + (f1 - f0)
                + " ms=" + ms);
    }

    static void churnPhase(String phase, long mb, int seed) {
        long y0 = count(true);
        long f0 = count(false);
        long t0 = System.nanoTime();
        long check = churn(mb, seed);
        report(phase, y0, f0, t0);
        System.out.println("EDEN-FOLLOW phase=" + phase + " mb=" + mb + " checksum=" + check);
    }

    public static void main(String[] args) {
        long churnMb = args.length > 0 ? Long.parseLong(args[0]) : 6000L;
        int retainMb = args.length > 1 ? Integer.parseInt(args[1]) : 600;
        sink += churn(256L, 7);
        churnPhase("start", churnMb, 1);

        long y0 = count(true);
        long f0 = count(false);
        long t0 = System.nanoTime();
        List<long[]> keep = retain(retainMb);
        System.gc();
        report("grow", y0, f0, t0);

        churnPhase("after", churnMb, 2);

        long sum = 0;
        for (long[] a : keep) {
            sum += a[0] + a[8189];
        }
        System.out.println("EDEN-FOLLOW retained=" + keep.size() + " sum=" + sum);
        if (sink == 42L) {
            System.err.println("[ed22] sink");
        }
        System.out.println("EDEN-FOLLOW-OK");
    }
}
