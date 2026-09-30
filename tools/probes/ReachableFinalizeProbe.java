import java.util.concurrent.atomic.AtomicInteger;

/**
 * A finalizable object that is UNAMBIGUOUSLY strongly reachable — held by a
 * static field and by a live local array — must never have `finalize()` run.
 *
 * No `ThreadLocal`, no JNI global ref, no dead thread: this isolates the
 * premature-finalization mechanism from the `ThreadLocal` root-leak the netty
 * investigation was about, so it keeps reproducing regardless of that fix.
 *
 * Expected on every collector: `finalized=0`, `PROBE-OK`.
 */
public class ReachableFinalizeProbe {
    static final AtomicInteger FINALIZED = new AtomicInteger(0);

    // A static field is a root the collector scans in its own dedicated pass.
    static Holder KEPT_STATIC;

    static final class Holder {
        final int id;

        Holder(int id) {
            this.id = id;
        }

        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    public static void main(String[] args) throws Exception {
        KEPT_STATIC = new Holder(1000);
        // A live local array: a frame root for the whole of main().
        Holder[] kept = new Holder[10];
        for (int i = 0; i < kept.length; i++) {
            kept[i] = new Holder(i);
        }

        for (int round = 0; round < 5; round++) {
            System.gc();
            Thread.sleep(200);
            System.out.println("round=" + round + " finalized=" + FINALIZED.get());
        }

        // Read every object back AFTER the collections, so nothing above can be
        // treated as dead early and the values prove the objects are intact.
        int sum = KEPT_STATIC.id;
        for (Holder h : kept) {
            sum += h.id;
        }
        System.out.println("sum=" + sum + " (expected 1045)");
        System.out.println("finalized=" + FINALIZED.get() + " (expected 0)");
        System.out.println(FINALIZED.get() == 0 && sum == 1045 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
