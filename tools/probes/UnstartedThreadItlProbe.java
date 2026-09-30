import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.locks.LockSupport;

/**
 * gc-common w5-b: a {@code Thread} constructed under an
 * {@code InheritableThreadLocal} context and never started must not keep its
 * inherited values alive once the {@code Thread} itself is unreachable.
 *
 * HotSpot stores the inherited map in {@code Thread.inheritableThreadLocals},
 * so it dies with the Thread: every weak reference below clears. CratonVM used
 * to queue the inherited values in a process-wide side table keyed by the
 * child's identity hash, drained only by the child's first ThreadLocal access
 * or its death path -- neither of which an unstarted Thread ever runs -- so
 * every value stayed a GC root for the life of the process
 * (docs/internal/gc-common-round-20260923/common-w4b-never-started-thread-inherited-tl-bucket-leaks-FIXED-20260923.md).
 *
 * Threads are built and dropped in batches so a VM that releases the buckets
 * stays small; a leaking VM accumulates THREADS * PAYLOAD bytes.
 */
public class UnstartedThreadItlProbe {
    static final int THREADS = 1000;
    static final int BATCH = 50;
    static final int PAYLOAD = 64 * 1024;

    static final List<WeakReference<byte[]>> CHILD_VALUES = new ArrayList<>();

    static final InheritableThreadLocal<byte[]> ITL = new InheritableThreadLocal<>() {
        @Override
        protected byte[] childValue(byte[] parent) {
            byte[] fresh = new byte[PAYLOAD];
            synchronized (CHILD_VALUES) {
                CHILD_VALUES.add(new WeakReference<>(fresh));
            }
            return fresh;
        }
    };

    static int live() {
        int n = 0;
        synchronized (CHILD_VALUES) {
            for (WeakReference<byte[]> r : CHILD_VALUES) {
                if (r.get() != null) {
                    n++;
                }
            }
        }
        return n;
    }

    public static void main(String[] args) {
        ITL.set(new byte[16]);
        for (int built = 0; built < THREADS; built += BATCH) {
            List<Thread> batch = new ArrayList<>();
            for (int i = 0; i < BATCH; i++) {
                batch.add(new Thread(() -> { }));
            }
            batch.clear();
        }
        int captured;
        synchronized (CHILD_VALUES) {
            captured = CHILD_VALUES.size();
        }
        System.out.println("constructed=" + THREADS + " childValue_calls=" + captured);
        long start = System.nanoTime();
        int gcCalls = 0;
        while (live() > 0 && System.nanoTime() - start < 30_000_000_000L) {
            System.gc();
            gcCalls++;
            LockSupport.parkNanos(50_000_000L);
        }
        int live = live();
        System.out.println("DONE live=" + live + " gcCalls=" + gcCalls);
        // captured == 0 would make the probe vacuous: the capture never ran.
        System.out.println(captured > 0 && live == 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
