import java.util.concurrent.atomic.AtomicInteger;

/**
 * Garbage finalizable objects must be finalized by ALLOCATION-DRIVEN
 * collections alone: this probe never calls System.gc() or
 * System.runFinalization().
 *
 * Guards gc-common w2-a (2026-09-23): the allocation doors (`maybe_gc`,
 * `maybe_gc_forced_at`) used to drop the collector's list of dead finalizable
 * objects it had resurrected, so each one was resurrected again - with its
 * whole subtree - on every allocation-triggered collection and finalize() never
 * ran until something called System.gc(). A program that never did so never
 * finalized anything and never freed a finalizable object.
 *
 * Output is deterministic: `created=N finalized=K` and one PROBE-OK / PROBE-FAIL
 * line. Runtime is bounded (TIME_LIMIT_MS of churn at most).
 *
 * PROBE-OK   every one of the N objects was finalized, exactly once.
 * PROBE-FAIL finalized < N within the time limit (the regression), or > N
 *            (a double finalization).
 *
 * The churn phase allocates plain garbage in a METHOD CALLED PER ROUND (not
 * one long loop) and checks progress between rounds, so an interpreter
 * allocation site is reached repeatedly even when the churn body is compiled.
 * If this probe fails only with the JIT on and passes under --nojit, the
 * collections were all allocation-failure collections from compiled code,
 * whose queued finalizers nothing drains - see
 * docs/internal/gc-common-round-20260923/common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door-FIXED-20260923.md.
 */
public class AllocDrivenFinalizeProbe {
    static final int N = 200;
    static final long TIME_LIMIT_MS = 20_000;
    static final AtomicInteger FINALIZED = new AtomicInteger(0);
    static volatile Object sink;

    static final class Finalizable {
        // A little payload so the subtree a resurrection keeps alive is real.
        final byte[] payload = new byte[256];

        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static void makeFinalizableGarbage() {
        for (int i = 0; i < N; i++) {
            new Finalizable();
        }
    }

    /** One round of plain garbage: ~8 MB in small arrays, a young-GC's worth. */
    static void churn() {
        for (int i = 0; i < 8 * 1024; i++) {
            sink = new byte[1024];
        }
    }

    public static void main(String[] args) throws Exception {
        makeFinalizableGarbage();
        long deadline = System.currentTimeMillis() + TIME_LIMIT_MS;
        int rounds = 0;
        while (FINALIZED.get() < N && System.currentTimeMillis() < deadline) {
            churn();
            rounds++;
            // Give a VM that runs finalizers on a separate thread (HotSpot's
            // Finalizer thread) a chance to run them; never asks for a GC.
            if ((rounds & 15) == 0) {
                Thread.sleep(10);
            }
        }
        // Settle: a few more rounds so a double finalization would show.
        for (int i = 0; i < 16; i++) {
            churn();
        }
        Thread.sleep(50);
        int n = FINALIZED.get();
        System.out.println("created=" + N + " finalized=" + n);
        if (n == N) {
            System.out.println("PROBE-OK");
        } else if (n > N) {
            System.out.println("PROBE-FAIL (double-finalized " + (n - N) + ")");
        } else {
            System.out.println("PROBE-FAIL (never finalized " + (N - n) + " without System.gc())");
        }
    }
}
