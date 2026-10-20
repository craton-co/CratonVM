// gcg g2/k: blocked-region hand-offs racing stop-the-world pauses.
//
// The workload `CRATONVM_STW_LOCKFREE_BLOCKED` changes: every `take`/`put` on
// a SynchronousQueue parks and unparks, i.e. enters and leaves a blocked
// region, while a collector thread allocates enough to force young
// collections (and a periodic System.gc()) so pauses keep landing in the
// middle of those transitions. Each hand-off carries a freshly allocated
// array, which the receiver checks after the wake: a thread the census
// excluded but that ran on through the pause (the defect the entry lock
// prevents) reads a relocated object through a stale reference.
//
//   GcgK2BlockedHandoffProbe <pairs> <rounds> <gcEvery>
//
// stdout (deterministic, diffed against HotSpot): one K2HANDOFF_OK line with
// the checksum of every payload received. stderr: wall time and hand-off rate.
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.atomic.AtomicLong;

public final class GcgK2BlockedHandoffProbe {
    static volatile boolean stop;
    static volatile Object sink;

    public static void main(String[] a) throws Exception {
        final int pairs = Integer.parseInt(a[0]);
        final int rounds = Integer.parseInt(a[1]);
        final int gcEvery = Integer.parseInt(a[2]);
        final AtomicLong sum = new AtomicLong();
        final AtomicLong bad = new AtomicLong();
        Thread[] ts = new Thread[pairs * 2];
        for (int p = 0; p < pairs; p++) {
            final int pair = p;
            final SynchronousQueue<long[]> q = new SynchronousQueue<>();
            ts[2 * p] = new Thread(() -> {
                try {
                    for (int i = 0; i < rounds; i++) {
                        long[] payload = new long[8];
                        for (int k = 0; k < payload.length; k++) {
                            payload[k] = (long) pair * 1_000_003L + i * 31L + k;
                        }
                        q.put(payload);
                    }
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }, "k2-put-" + p);
            ts[2 * p + 1] = new Thread(() -> {
                try {
                    long local = 0;
                    for (int i = 0; i < rounds; i++) {
                        long[] payload = q.take();
                        for (int k = 0; k < payload.length; k++) {
                            long want = (long) pair * 1_000_003L + i * 31L + k;
                            if (payload[k] != want) {
                                bad.incrementAndGet();
                            }
                            local += payload[k];
                        }
                    }
                    sum.addAndGet(local);
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }, "k2-take-" + p);
        }
        Thread gc = new Thread(() -> {
            int n = 0;
            while (!stop) {
                sink = new byte[4096];
                if (++n % gcEvery == 0) {
                    System.gc();
                }
            }
        }, "k2-alloc");
        gc.setDaemon(true);
        long t0 = System.nanoTime();
        gc.start();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        stop = true;
        gc.join();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("K2HANDOFF_OK pairs=" + pairs + " rounds=" + rounds
                + " sum=" + sum.get() + " bad=" + bad.get());
        System.err.println("[k2-handoff] ms=" + ms + " handoffs=" + ((long) pairs * rounds)
                + " per_ms=" + (ms == 0 ? 0 : ((long) pairs * rounds) / ms));
    }
}
