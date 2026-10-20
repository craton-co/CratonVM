import java.lang.ref.WeakReference;

/**
 * gci i31/gq (docs/known-issues/gc/gci-i30rp-g1-dead-finalizable-in-a-kept-young-region-is-not-finalized-at-the-pause-20261015.md):
 * a dead finalizable {@code Fin} allocated right beside a {@code Box} that a
 * compiled loop holds for its whole run (one TLAB, so one region, which a G1
 * young pause of this VM keeps in place while the compiled frame names the
 * box). A {@code WeakReference} to the finalizable is the observable: HotSpot
 * clears it at the first young pause that finds the object dead, before its
 * {@code finalize()} runs, whatever region it is in.
 *
 * <p>CratonVM G1 before i31/gq: the finalizable stayed registered in the kept
 * region and read live region-wide, so the weak reference survived every pause
 * of the loop (the loop runs to its deadline). No {@code System.gc()}: a churn
 * thread forces the young pauses.
 *
 * <p>stdout (deterministic, HotSpot 25 with any collector): {@code fact
 * cleared-while-pinned true}, {@code fact finalized true}, {@code fact
 * warm-kept true}. Diagnostics on stderr ({@code [gq31p] ...}).
 *
 * <p>Mode {@code reach} (second argument) is the guard: the same finalizable
 * is also held by a static array, so it must never be finalized and its weak
 * reference never cleared; stdout {@code fact reachable-kept true}, {@code fact
 * warm-kept true} (the loop runs to its deadline on every VM).
 *
 * <p>Args: {@code [deadline seconds (default 10)] [dead|reach (default dead)]}.
 */
public class GciI31GqPinnedFinalizableProbe {
    static volatile boolean stop;
    static volatile long sinkLong;
    static volatile Object sinkObj;
    static volatile int finalized;
    static volatile Fin saved;
    static final Object KEPT = new Object();
    /** Strongly held for the whole run: the warm-up's weak referent. */
    static final Fin WARM_FIN = new Fin();
    static Box pinStash;
    static WeakReference<Fin> weakStash;
    /** reach mode: a static array (an old object by the time of the pauses) holds the finalizable. */
    static final Object[] HOLD = new Object[1];

    static final class Box {
        int v = 7;
    }

    static final class Fin {
        final int[] payload = new int[] {3, 5, 9};

        @Override
        @SuppressWarnings({"deprecation", "removal"})
        protected void finalize() {
            saved = this;
            finalized = finalized + 1;
        }
    }

    /** The referent never stays in the caller's frame: only the answer does. */
    static boolean gone(WeakReference<?> w) {
        return w.get() == null;
    }

    /**
     * The compiled loop: holds {@code pin} (and so its region) for its whole
     * run; returns 1 once the finalizable's weak reference reads null, 2 if the
     * control reference to a static object does (never expected), 0 at the
     * deadline.
     */
    static int spin(Box pin, WeakReference<Fin> wf, WeakReference<Object> k, long deadlineNanos) {
        long acc = 1;
        for (long i = 0; ; i++) {
            acc = acc * 6364136223846793005L + i;
            if ((i & 0xFFFFFL) == 0) {
                acc ^= pin.v;
                if (gone(k)) {
                    sinkLong = acc;
                    return 2;
                }
                if (gone(wf)) {
                    sinkLong = acc;
                    return 1;
                }
                if (System.nanoTime() > deadlineNanos) {
                    sinkLong = acc;
                    return 0;
                }
            }
        }
    }

    /**
     * The box, then at once the finalizable and its weak reference (one TLAB).
     * Only the box and the weak reference leave this frame, through statics.
     */
    static void fresh(boolean reach) {
        Box pin = new Box();
        Fin f = new Fin();
        WeakReference<Fin> wf = new WeakReference<>(f);
        if (reach) {
            HOLD[0] = f;
        }
        pinStash = pin;
        weakStash = wf;
    }

    public static void main(String[] args) throws Exception {
        long seconds = args.length > 0 ? Long.parseLong(args[0]) : 10L;
        boolean reach = args.length > 1 && args[1].equals("reach");
        WeakReference<Object> k = new WeakReference<>(KEPT);
        WeakReference<Fin> warmWeak = new WeakReference<>(WARM_FIN);
        Box warmBox = new Box();
        for (int i = 0; i < 200; i++) {
            spin(warmBox, warmWeak, k, System.nanoTime() + 2_000_000L);
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
        fresh(reach);
        Box pin = pinStash;
        WeakReference<Fin> wf = weakStash;
        pinStash = null;
        weakStash = null;
        churn.start();
        long t0 = System.nanoTime();
        int result = spin(pin, wf, k, t0 + seconds * 1_000_000_000L);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        int waits = 0;
        // reach: a few drains only -- a finalizer wrongly queued would run here.
        int maxWaits = reach ? 6 : 200;
        while (finalized == 0 && waits < maxWaits) {
            System.runFinalization();
            Thread.sleep(50);
            waits++;
        }
        stop = true;
        churn.join();
        Fin back = saved;
        boolean intact = back != null
                && back.payload.length == 3
                && back.payload[0] + back.payload[1] + back.payload[2] == 17;
        if (reach) {
            System.out.println("fact reachable-kept "
                    + (result == 0 && finalized == 0 && HOLD[0] != null && !gone(wf)));
        } else {
            System.out.println("fact cleared-while-pinned " + (result == 1));
            System.out.println("fact finalized " + (finalized == 1 && intact));
        }
        System.out.println("fact warm-kept " + (result != 2 && !gone(k)));
        System.err.println("[gq31p] result=" + result + " elapsed_ms=" + ms + " finalize_waits=" + waits
                + " finalized=" + finalized + " pin=" + pin.v + " still_cleared=" + gone(wf));
    }
}
