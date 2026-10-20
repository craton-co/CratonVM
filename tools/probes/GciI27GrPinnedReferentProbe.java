import java.lang.ref.WeakReference;

/**
 * gci i27/gr (docs/known-issues/gc/jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-20261013.md):
 * a weak referent that dies beside its own {@code WeakReference} (one TLAB, one
 * region) while a compiled loop holds the {@code WeakReference} for its whole
 * run. No {@code System.gc()}: a churn thread forces young pauses while the
 * compiled {@code spin} runs. HotSpot 25 (any collector) clears the referent at
 * the first young pause; CratonVM G1 kept every referent of a region a pause
 * left in place (a JIT pin, a frozen peer's TLAB tail), so only a marking cycle
 * could clear it.
 *
 * <p>stdout (deterministic, HotSpot): {@code fact cleared true} and
 * {@code fact warm-kept true}. Diagnostics (rounds, elapsed) on stderr.
 *
 * <p>Args: {@code [iterations per spin call (default 100000000)] [max rounds (default 50)]}.
 */
public class GciI27GrPinnedReferentProbe {
    static volatile boolean stop;
    static volatile long sinkLong;
    static volatile Object sinkObj;
    static final Object KEPT = new Object();

    /** The referent never stays in the caller's frame: only the answer does. */
    static boolean gone(WeakReference<Object> w) {
        return w.get() == null;
    }

    /** The compiled loop: holds {@code w} and {@code k} for its whole run. */
    static int spin(WeakReference<Object> w, WeakReference<Object> k, long iters) {
        long acc = 1;
        for (long i = 0; i < iters; i++) {
            acc = acc * 6364136223846793005L + i;
            if ((i & 0xFFFFFL) == 0) {
                if (gone(k)) {
                    sinkLong = acc;
                    return 2;
                }
                if (gone(w)) {
                    sinkLong = acc;
                    return 1;
                }
            }
        }
        sinkLong = acc;
        return 0;
    }

    static WeakReference<Object> fresh() {
        return new WeakReference<>(new Object());
    }

    public static void main(String[] args) throws Exception {
        long iters = args.length > 0 ? Long.parseLong(args[0]) : 100_000_000L;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 50;
        WeakReference<Object> k = new WeakReference<>(KEPT);
        WeakReference<Object> warm = new WeakReference<>(KEPT);
        for (int i = 0; i < 200; i++) {
            spin(warm, k, 20_000L);
        }
        Thread churn = new Thread(() -> {
            Object[] ring = new Object[64];
            int n = 0;
            while (!stop) {
                ring[n++ & 63] = new byte[2048];
            }
            sinkObj = ring;
        }, "churn");
        churn.setDaemon(true);
        churn.start();
        WeakReference<Object> w = fresh();
        long t0 = System.nanoTime();
        int result = 0;
        int round = 0;
        while (round < rounds && result == 0) {
            result = spin(w, k, iters);
            round++;
        }
        stop = true;
        churn.join();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("fact cleared " + (result == 1));
        System.out.println("fact warm-kept " + (result != 2 && !gone(k)));
        System.err.println("[gr27] result=" + result + " rounds=" + round + " elapsed_ms=" + ms);
    }
}
