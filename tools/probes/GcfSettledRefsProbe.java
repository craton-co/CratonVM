import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gcf f2/r (2026-10-01): the pause cost of SETTLED reference rows -- cleared
 * and delivered {@code WeakReference}s the program keeps
 * (gcf-f1r-proposal-settled-reference-rows-leave-the-per-pause-walks).
 *
 * <p>Creates {@code n} (default 200000) weak references, half with a queue,
 * whose referents die; one {@code System.gc()} clears them all and the queue
 * is drained. Then {@code rounds} (default 40) rounds of short-lived garbage
 * run, each forcing young collections. Mode {@code keep} (default) keeps the
 * cleared references in an array through the churn; mode {@code drop} drops
 * the array first (the proposal's ceiling: the same churn with no settled
 * rows). A second set of {@code live} (1000) weak references whose referents
 * stay strongly reachable must survive every collection.
 *
 * <p>Stdout (deterministic, identical on HotSpot): {@code cleared=<n>
 * delivered=<n/2> live=1000/1000 PROBE-OK}. Stderr: {@code churn-ms=<t>}, the
 * churn's wall time; compare it (and the pause totals of {@code -Xlog:gc} /
 * {@code CRATONVM_GC_STATS=1}) between modes and switch settings.
 */
public class GcfSettledRefsProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "keep";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 40;

        ReferenceQueue<Object> q = new ReferenceQueue<>();
        WeakReference<?>[] settled = new WeakReference<?>[n];
        for (int i = 0; i < n; i++) {
            Object referent = new int[4];
            settled[i] = (i & 1) == 0 ? new WeakReference<>(referent, q) : new WeakReference<>(referent);
        }
        Object[] liveReferents = new Object[1000];
        WeakReference<?>[] live = new WeakReference<?>[liveReferents.length];
        for (int i = 0; i < live.length; i++) {
            liveReferents[i] = new int[] {i};
            live[i] = new WeakReference<>(liveReferents[i]);
        }

        System.gc();
        int cleared = 0;
        for (int attempt = 0; attempt < 50; attempt++) {
            cleared = 0;
            for (WeakReference<?> r : settled) {
                if (r.get() == null) {
                    cleared++;
                }
            }
            if (cleared == n) {
                break;
            }
            System.gc();
            Thread.sleep(20);
        }
        int delivered = 0;
        while (delivered < n / 2) {
            if (q.remove(5000) == null) {
                break;
            }
            delivered++;
        }
        if (mode.equals("drop")) {
            settled = null;
        }

        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 200_000; i++) {
                sink = new byte[64];
            }
        }
        long churnMs = (System.nanoTime() - t0) / 1_000_000;

        int liveOk = 0;
        for (int i = 0; i < live.length; i++) {
            if (live[i].get() == liveReferents[i]) {
                liveOk++;
            }
        }
        System.err.println("churn-ms=" + churnMs + " mode=" + mode + " kept=" + (settled != null));
        System.out.println("cleared=" + cleared + " delivered=" + delivered + " live=" + liveOk + "/"
                + live.length + (cleared == n && delivered == n / 2 && liveOk == live.length ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
