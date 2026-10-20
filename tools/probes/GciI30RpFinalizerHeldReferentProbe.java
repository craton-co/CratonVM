import java.lang.ref.WeakReference;

/**
 * gci i30/rp (i27-gr-report.md review item 4): a weak referent {@code W} that
 * lives beside its own {@code WeakReference} (one TLAB, one region the compiled
 * {@code spin} pins by holding the {@code WeakReference}), reachable only through
 * a dead finalizable {@code Holder} allocated in ANOTHER region. The holder's
 * {@code finalize()} resurrects it for good (stores {@code this} in a static),
 * so after finalization {@code W} is strongly reachable again.
 *
 * <p>HotSpot 25 (any collector): the first young pause clears the weak reference
 * before the holder is finalized (finalizer-reachable is not weakly reachable);
 * the resurrection does not bring it back. CratonVM G1 before i30/rp: the
 * holder's resurrected copy was a holder of {@code W} in the kept region, so the
 * reference survived the pause and, once {@code finalize()} resurrected the
 * holder, was never cleared.
 *
 * <p>No {@code System.gc()}: a churn thread forces young pauses. stdout
 * (deterministic, HotSpot): {@code fact cleared true}, {@code fact finalized true},
 * {@code fact warm-kept true}. Diagnostics on stderr ({@code [rp30] ...}).
 *
 * <p>Args: {@code [iterations per spin call (default 100000000)] [max rounds (default 50)]}.
 */
public class GciI30RpFinalizerHeldReferentProbe {
    static volatile boolean stop;
    static volatile long sinkLong;
    static volatile Object sinkObj;
    static volatile Holder saved;
    static final Object KEPT = new Object();

    static final class Holder {
        Object payload;

        Holder(Object payload) {
            this.payload = payload;
        }

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            saved = this;
        }
    }

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

    /**
     * {@code W} and its {@code WeakReference} back to back (one TLAB), then about
     * 4 MiB of garbage so the holder lands in another region, then the holder.
     * Only the {@code WeakReference} is returned.
     */
    static WeakReference<Object> fresh() {
        Object referent = new Object();
        WeakReference<Object> w = new WeakReference<>(referent);
        for (int i = 0; i < 64; i++) {
            sinkObj = new byte[64 * 1024];
        }
        sinkObj = null;
        new Holder(referent);
        return w;
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
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        // The holder's finalize() must have run (it resurrects the holder); wait
        // for it with the churn still forcing pauses, at most ~20 s.
        int waits = 0;
        while (saved == null && waits < 200) {
            System.runFinalization();
            Thread.sleep(100);
            waits++;
        }
        stop = true;
        churn.join();
        Holder h = saved;
        boolean stillCleared = gone(w);
        System.out.println("fact cleared " + (result == 1 && stillCleared));
        System.out.println("fact finalized " + (h != null && h.payload != null));
        System.out.println("fact warm-kept " + (result != 2 && !gone(k)));
        System.err.println("[rp30] result=" + result + " rounds=" + round + " elapsed_ms=" + ms
                + " finalize_waits=" + waits + " cleared_after_resurrection=" + stillCleared);
    }
}
