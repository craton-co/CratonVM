import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * Probe matching LongRandomBinaryDocValuesRangeQueryTests.testAllEqual's
 * shape: one initial write-lock cycle, then sustained multi-threaded
 * read-lock-only contention. Historically completes on both fixed and
 * unfixed binaries (does not reproduce the original hang standalone) --
 * kept as a sanity check.
 *
 * Usage: RwlReadDominantProbe <numReaderThreads> <durationMs>
 */
public class RwlReadDominantProbe {
    static final ReentrantReadWriteLock rwl = new ReentrantReadWriteLock();
    static long shared = 0;
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int numThreads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        long durationMs = args.length > 1 ? Long.parseLong(args[1]) : 10000;

        rwl.writeLock().lock();
        try {
            shared = 42;
        } finally {
            rwl.writeLock().unlock();
        }

        Thread[] readers = new Thread[numThreads];
        final long[] sums = new long[numThreads];
        for (int i = 0; i < numThreads; i++) {
            final int idx = i;
            readers[i] = new Thread(() -> {
                long sum = 0;
                while (!stop) {
                    rwl.readLock().lock();
                    try {
                        sum += shared;
                    } finally {
                        rwl.readLock().unlock();
                    }
                }
                sums[idx] = sum;
            }, "reader-" + i);
        }

        for (Thread t : readers) t.start();
        Thread.sleep(durationMs);
        stop = true;
        for (Thread t : readers) t.join();

        long total = 0;
        for (long s : sums) total += s;
        System.out.println("DONE total=" + total + " sharedFinal=" + shared);
    }
}
