/**
 * gci i28/wt (docs/known-issues/gc/gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-20261015.md):
 * the JNI twin of {@code GciI27GrPinnedReferentProbe}. A JNI weak global to an
 * object that dies beside its holder (one TLAB, one region) while a compiled
 * loop holds the holder for its whole run. No {@code System.gc()}: a churn
 * thread forces young pauses while the compiled {@code spin} runs. HotSpot 25
 * (any collector) clears the weak global at the first young pause
 * ({@code IsSameObject(w, NULL)} answers true); CratonVM G1 kept every weak
 * global whose referent lay in a young region a pause left in place (a JIT pin,
 * a frozen peer's TLAB tail), until a pause evacuated the region.
 *
 * <p>stdout (deterministic, HotSpot): {@code fact cleared true} and
 * {@code fact kept-alive true}. Diagnostics (rounds, elapsed) on stderr.
 *
 * <p>Args: {@code <absolute path of libgcii28wt.so> [iterations per spin call
 * (default 100000000)] [max rounds (default 50)]}.
 */
public class GciI28WtJniWeakPinnedProbe {
    static volatile boolean stop;
    static volatile long sinkLong;
    static volatile Object sinkObj;
    static final Object KEPT = new Object();

    static native void makeWeak(int slot, Object o);

    static native boolean weakCleared(int slot);

    static native void deleteWeak(int slot);

    /** The compiled loop: holds {@code holder} for its whole run. */
    static int spin(Object[] holder, long iters) {
        long acc = 1;
        for (long i = 0; i < iters; i++) {
            acc = acc * 6364136223846793005L + i;
            if ((i & 0xFFFFFL) == 0) {
                acc += holder.length;
                if (weakCleared(1)) {
                    sinkLong = acc;
                    return 2;
                }
                if (weakCleared(0)) {
                    sinkLong = acc;
                    return 1;
                }
            }
        }
        sinkLong = acc;
        return 0;
    }

    /**
     * The holder and the referent, allocated back to back; the referent never
     * leaves this frame except as the weak global's target.
     */
    static Object[] fresh() {
        Object[] holder = new Object[1];
        Object referent = new Object();
        makeWeak(0, referent);
        return holder;
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        long iters = args.length > 1 ? Long.parseLong(args[1]) : 100_000_000L;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 50;
        makeWeak(1, KEPT);
        Object[] warm = new Object[1];
        for (int i = 0; i < 200; i++) {
            spin(warm, 20_000L);
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
        Object[] holder = fresh();
        long t0 = System.nanoTime();
        int result = 0;
        int round = 0;
        while (round < rounds && result == 0) {
            result = spin(holder, iters);
            round++;
        }
        stop = true;
        churn.join();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("fact cleared " + (result == 1));
        System.out.println("fact kept-alive " + (result != 2 && !weakCleared(1)));
        System.err.println("[wt28] result=" + result + " rounds=" + round + " elapsed_ms=" + ms
                + " holder=" + holder.length);
        deleteWeak(0);
        deleteWeak(1);
    }
}
