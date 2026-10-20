import java.util.concurrent.atomic.AtomicInteger;

/**
 * Garbage finalizable objects must each have `finalize()` run EXACTLY ONCE,
 * across repeated collections.
 *
 * Guards the two directions the young sweep's finalizer-resurrection phase can
 * get wrong: never reporting a dead object (finalized < created) and reporting
 * one that is already queued or already finalized again (finalized > created).
 */
public class FinalizeOnceProbe {
    static final int N = 20;
    static final AtomicInteger FINALIZED = new AtomicInteger(0);

    static final class Garbage {
        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static void makeGarbage() {
        for (int i = 0; i < N; i++) {
            new Garbage();
        }
    }

    public static void main(String[] args) throws Exception {
        makeGarbage();
        // Several collections: a double-report shows up as a count over N.
        for (int round = 0; round < 8; round++) {
            System.gc();
            Thread.sleep(150);
        }
        int n = FINALIZED.get();
        System.out.println("created=" + N + " finalized=" + n);
        if (n == N) {
            System.out.println("PROBE-OK");
        } else if (n > N) {
            System.out.println("PROBE-FAIL (double-finalized " + (n - N) + ")");
        } else {
            System.out.println("PROBE-FAIL (never finalized " + (N - n) + ")");
        }
    }
}
