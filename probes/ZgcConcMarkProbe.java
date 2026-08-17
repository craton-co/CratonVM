import java.util.*;

/**
 * The workload ZGC's concurrent marking is FOR: a large stable live set that
 * makes the mark phase dominate the pause, plus a steady allocation rate that
 * reaches the collection threshold on its own.
 *
 * <p>{@code BigLive} calls {@code System.gc()}, which is why it cannot measure
 * this: a forced collection arrives without ever crossing the concurrent-start
 * threshold, so the cycle never opens and every pause is a stop-the-world mark.
 * Here every collection is allocation-triggered, which is the only path that
 * reaches {@code maybe_gc}'s concurrent-start check.
 *
 * <p>It also STORES references into the live set while allocating, so the SATB
 * pre-write barrier has real work: a run whose {@code ingress_replayed} is zero
 * has not exercised the barrier at all, and would report a pause improvement
 * that a store-heavy application would not see.
 *
 * <p>Read the result off {@code --verbose:gc}'s {@code mark=} field:
 * {@code mark=concurrent} means the closure was traced with this thread
 * running, {@code mark=stw-parallel} / {@code mark=stw-serial} that it was not.
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx1500m --verbose:gc ZgcConcMarkProbe 4000 250 40
 * </pre>
 *
 * args: width, depth, rounds
 */
public class ZgcConcMarkProbe {
    static Object[] retained;

    public static void main(String[] args) {
        int width  = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        int depth  = args.length > 1 ? Integer.parseInt(args[1]) : 250;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 40;

        // ---- the live set ------------------------------------------------
        retained = new Object[width];
        for (int i = 0; i < width; i++) {
            Object[] head = new Object[4];
            Object[] cur = head;
            for (int d = 0; d < depth; d++) {
                Object[] next = new Object[4];
                next[0] = new byte[64];
                cur[1] = next;
                cur = next;
            }
            retained[i] = head;
        }
        long liveNodes = (long) width * depth;
        System.out.println("live nodes ~" + liveNodes);

        // ---- allocate + store, until the collector triggers on its own ----
        long t0 = System.nanoTime();
        long stores = 0;
        Object sink = null;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < 60_000; k++) {
                Object o = new byte[128];
                sink = o;
                // A reference store into the LIVE set. Slot 2 is unused by the
                // chain above, so overwriting it cannot shorten the graph --
                // the live set stays the same size for every round, which is
                // what makes the per-round pause figures comparable.
                if ((k & 15) == 0) {
                    Object[] head = (Object[]) retained[(int) (stores % width)];
                    head[2] = o;
                    stores++;
                }
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        // Keep everything reachable to the very end so no round can be
        // measuring a smaller live set than the one it was given.
        long check = 0;
        for (int i = 0; i < width; i++) {
            if (retained[i] != null) check++;
        }
        System.out.println("rounds=" + rounds + " stores=" + stores
                + " retained_live=" + check + " sink_null=" + (sink == null)
                + " elapsed_ms=" + ms);
    }
}
