import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gci i43/gy -- G1's young length under -Xmn / -XX:NewSize / -XX:MaxNewSize /
 * -XX:NewRatio (`docs/known-issues/gc/gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-20261016.md`).
 *
 * <p>A fixed live set (kept for the whole run) and a stream of short-lived
 * 256-byte arrays. The young collection count is then decided by the young
 * length alone: HotSpot collects when Eden reaches its target, so with -Xmn32m
 * a 2 GiB stream is ~64 young pauses; a young target that ignores -Xmn
 * collects at the Free-pool trigger instead (several times fewer).
 *
 * <p>stdout is deterministic (the HotSpot diff); the per-collector counts go
 * to stderr as {@code [gy43] gc=<name> count=<n>}.
 *
 * <p>Usage: {@code GciGy43YoungLengthProbe [garbageMiB=2048] [liveMiB=8]}
 */
public final class GciGy43YoungLengthProbe {

    /** Every 1024th array escapes here, so no allocation can be elided. */
    static volatile Object sink;

    public static void main(String[] args) {
        int garbageMiB = args.length > 0 ? Integer.parseInt(args[0]) : 2048;
        int liveMiB = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        final int chunk = 256;
        final int payload = chunk - 16;

        Object[] live = new Object[(liveMiB * 1024 * 1024) / chunk];
        for (int i = 0; i < live.length; i++) {
            live[i] = new byte[payload];
        }

        long sum = 0;
        long n = ((long) garbageMiB * 1024L * 1024L) / chunk;
        for (long i = 0; i < n; i++) {
            byte[] b = new byte[payload];
            b[0] = (byte) i;
            sum += b[0] + b.length;
            if ((i & 1023L) == 0L) {
                sink = b;
            }
        }
        long liveLen = 0;
        for (Object o : live) {
            liveLen += ((byte[]) o).length;
        }

        System.out.println("GciGy43YoungLengthProbe garbageMiB=" + garbageMiB
                + " liveMiB=" + liveMiB
                + " sum=" + sum
                + " liveBytes=" + liveLen);
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            System.err.println("[gy43] gc=" + gc.getName().replace(' ', '_')
                    + " count=" + gc.getCollectionCount());
        }
    }
}
