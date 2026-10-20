import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

/**
 * gcg g5/xt (`common-w28a-proposal-shard-the-lock-key-registry-REFUTED-20261015`): the
 * contention measurement `CRATONVM_LOCK_KEY_SHARDS` never had. Each of N
 * threads owns its OWN StampedLock and ReentrantReadWriteLock and cycles
 * read/write lock-unlock pairs on them. No two threads share a lock, so on
 * HotSpot the threads never contend; under `--compatible` every operation
 * looks its lock up in the native lock-key registry, which is one mutex with
 * the switch off and 64 shards with it on.
 *
 * stdout (deterministic, compared with HotSpot): one line
 * {@code K5LKS_OK threads=<n> iters=<m> sum=<s> bad=0}.
 * stderr: {@code [k5-lks] ms=<wall>} (the timing pair's number).
 *
 * Args: threads (default 16), iterations per thread (default 200000).
 */
public final class GcgG5LockKeyContentionProbe {
    public static void main(String[] a) throws Exception {
        final int threads = a.length > 0 ? Integer.parseInt(a[0]) : 16;
        final int iters = a.length > 1 ? Integer.parseInt(a[1]) : 200_000;
        final long[] sums = new long[threads];
        final int[] bads = new int[threads];
        final CountDownLatch start = new CountDownLatch(1);
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                StampedLock sl = new StampedLock();
                ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
                long sum = 0;
                int bad = 0;
                try {
                    start.await();
                } catch (InterruptedException e) {
                    bad++;
                }
                for (int i = 0; i < iters; i++) {
                    long s = sl.readLock();
                    sum += (i & 7);
                    sl.unlockRead(s);
                    long w = sl.writeLock();
                    sum += 1;
                    sl.unlockWrite(w);
                    rw.readLock().lock();
                    rw.readLock().unlock();
                    if ((i & 1023) == 0) {
                        rw.writeLock().lock();
                        if (!rw.isWriteLockedByCurrentThread()) bad++;
                        rw.writeLock().unlock();
                    }
                }
                if (sl.isWriteLocked() || sl.isReadLocked()) bad++;
                sums[id] = sum;
                bads[id] = bad;
            }, "k5-lks-" + t);
            ts[t].start();
        }
        long t0 = System.nanoTime();
        start.countDown();
        for (Thread t : ts) {
            t.join();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        long sum = 0;
        int bad = 0;
        for (int t = 0; t < threads; t++) {
            sum += sums[t];
            bad += bads[t];
        }
        System.err.println("[k5-lks] ms=" + ms);
        System.out.println("K5LKS_OK threads=" + threads + " iters=" + iters + " sum=" + sum + " bad=" + bad);
    }
}
