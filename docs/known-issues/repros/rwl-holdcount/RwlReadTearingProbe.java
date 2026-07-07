import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * Repro probe for the JIT-specific ReentrantReadWriteLock reader-vs-writer
 * hang (elasticsearch-lucene-binary-docvalues-range-hangs.md, residual after
 * root cause #3). Shape: sustained reader threads PLUS a continuously-cycling
 * writer. Reliably hangs under CratonVM JIT; completes under --nojit and on
 * HotSpot.
 *
 * Usage: RwlReadTearingProbe <numReaderThreads> <durationMs>
 */
public class RwlReadTearingProbe {
    static final ReentrantReadWriteLock rwl = new ReentrantReadWriteLock();
    static long shared = 0;
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        System.out.println("MAIN_START " + System.currentTimeMillis());
        int numThreads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        long durationMs = args.length > 1 ? Long.parseLong(args[1]) : 10000;

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
                System.out.println("READER_EXIT " + idx + " " + System.currentTimeMillis());
            }, "reader-" + i);
        }
        Thread writer = new Thread(() -> {
            long v = 0;
            while (!stop) {
                rwl.writeLock().lock();
                try {
                    shared = ++v;
                } finally {
                    rwl.writeLock().unlock();
                }
            }
            System.out.println("WRITER_EXIT " + System.currentTimeMillis());
        }, "writer");

        for (Thread t : readers) t.start();
        writer.start();
        System.out.println("THREADS_STARTED " + System.currentTimeMillis());
        Thread.sleep(durationMs);
        stop = true;
        System.out.println("STOP_SET " + System.currentTimeMillis());
        for (Thread t : readers) t.join();
        writer.join();

        System.out.println("JOINED " + System.currentTimeMillis());
        long total = 0;
        for (long s : sums) total += s;
        System.out.println("DONE total=" + total + " sharedFinal=" + shared);
    }
}
