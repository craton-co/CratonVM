import java.util.concurrent.atomic.AtomicInteger;

/**
 * Garbage finalizable objects must be finalized when EVERY collection after
 * they die is caused by COMPILED code: this probe never calls System.gc() or
 * System.runFinalization(), and after the finalizable objects are created the
 * main thread performs no interpreted allocation at all.
 *
 * Guards gc-common w5-a (2026-09-23), the fully-compiled half of
 * docs/internal/gc-common-round-20260923/common-w2a-finalizers-queued-by-forced-collections-wait-for-another-door-FIXED-20260923.md
 * (moved to docs/internal/gc-common-round-20260923/ once fixed). A compiled
 * allocation loop collects only through the allocation-FAILURE door, which
 * cannot run Java, and never reaches the interpreter's no-collection drain -
 * the one place that ran the queue for it. Every finalizer it queued waited
 * (re-rooted with its whole subtree on every collection) for an interpreted
 * allocation that never came. AllocDrivenFinalizeProbe does not show this: its
 * churn is reached from an interpreted caller between rounds.
 *
 * Shape: warm `churn` until it is compiled (tiny calls, so the warm-up's own
 * collections are few), create N finalizable objects and drop them, then
 * churn in compiled code only. HotSpot finalizes all N on its Finalizer
 * thread; CratonVM hands finalizers no door drained for two whole pauses to
 * its reference-delivery thread.
 *
 * Output is deterministic: `created=N finalized=K` and one PROBE-OK /
 * PROBE-FAIL line. Runtime is bounded (TIME_LIMIT_MS of churn at most).
 * With --nojit this probe exercises the interpreted drain instead and must
 * also print PROBE-OK.
 */
public class CompiledOnlyFinalizeProbe {
    static final int N = 200;
    static final long TIME_LIMIT_MS = 20_000;
    static final int WARM_CALLS = 50_000;
    static final AtomicInteger FINALIZED = new AtomicInteger(0);
    static volatile Object sink;

    static final class Finalizable {
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

    /** `n` small arrays of plain garbage; 8192 is ~8 MB, a young GC's worth. */
    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            sink = new byte[1024];
        }
    }

    public static void main(String[] args) throws Exception {
        // Warm-up: get `churn` compiled while there is nothing to finalize.
        for (int i = 0; i < WARM_CALLS; i++) {
            churn(1);
        }
        makeFinalizableGarbage();
        long deadline = System.currentTimeMillis() + TIME_LIMIT_MS;
        int rounds = 0;
        // No allocation on this loop's own (interpreted) path.
        while (FINALIZED.get() < N && System.currentTimeMillis() < deadline) {
            churn(8 * 1024);
            rounds++;
            if ((rounds & 15) == 0) {
                // HotSpot's Finalizer thread needs a moment; this sleep is a
                // blocking region in CratonVM and drains nothing itself.
                Thread.sleep(10);
            }
        }
        // Settle: a few more rounds so a double finalization would show.
        for (int i = 0; i < 16; i++) {
            churn(8 * 1024);
        }
        Thread.sleep(50);
        int n = FINALIZED.get();
        System.out.println("created=" + N + " finalized=" + n);
        if (n == N) {
            System.out.println("PROBE-OK");
        } else if (n > N) {
            System.out.println("PROBE-FAIL (double-finalized " + (n - N) + ")");
        } else {
            System.out.println("PROBE-FAIL (never finalized " + (N - n)
                    + " with only compiled code collecting)");
        }
    }
}
