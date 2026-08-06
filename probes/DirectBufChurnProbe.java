import java.nio.ByteBuffer;
import java.util.concurrent.*;

/**
 * Exercises the reclaim-and-retry path in java.nio.Bits.reserveMemory from
 * MULTIPLE threads, where the buffers are still live at the instant of a
 * refusal and are released microseconds later by another thread.
 *
 * That is the case the single-threaded DirectBufProbe cannot reach: there, a
 * forced collection always frees everything at round=0. Here the allocating
 * thread should have to wait for a peer, which is what the exponential
 * back-off exists for.
 */
public class DirectBufChurnProbe {
    static final int THREADS = 8;
    static final int PER_BUF_MIB = 16;
    static final int ROUNDS = 40;
    static final long HOLD_MS = 30;

    public static void main(String[] args) throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(THREADS);
        CountDownLatch done = new CountDownLatch(THREADS);
        final long[] allocated = new long[THREADS];
        final Throwable[] failure = new Throwable[1];

        for (int t = 0; t < THREADS; t++) {
            final int id = t;
            pool.submit(() -> {
                try {
                    for (int r = 0; r < ROUNDS; r++) {
                        ByteBuffer b = ByteBuffer.allocateDirect(PER_BUF_MIB * 1024 * 1024);
                        b.putInt(0, r);
                        allocated[id] += PER_BUF_MIB;
                        // hold it long enough that peers see the cap while it
                        // is still live, then drop it
                        Thread.sleep(HOLD_MS);
                        b = null;
                    }
                } catch (Throwable e) {
                    synchronized (failure) {
                        if (failure[0] == null) failure[0] = e;
                    }
                } finally {
                    done.countDown();
                }
            });
        }

        done.await();
        pool.shutdown();
        pool.awaitTermination(60, TimeUnit.SECONDS);

        long total = 0;
        for (long a : allocated) total += a;
        if (failure[0] != null) {
            System.out.println("FAIL " + failure[0]);
            failure[0].printStackTrace();
        } else {
            System.out.println("OK threads=" + THREADS + " totalAllocatedMiB=" + total);
        }
    }
}
