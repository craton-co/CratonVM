// gcg g2/k: contended ThreadLocal.get() hits, the path
// `CRATONVM_TL_GET_ONE_GLOBAL_REF_LOCK` shortens (one global-ref lock per hit
// instead of two).
//
// Every thread reads its own ThreadLocal value in a tight loop and takes a
// shared ReentrantReadWriteLock's read lock (whose contended path reaches
// `readHolds.get()`), with a little allocation so collections run and the
// rows' handles are remapped between hits.
//
//   GcgK2ThreadLocalGetProbe <threads> <iters>
//
// stdout (deterministic): one K2TLGET_OK line with a checksum.
// stderr: wall time.
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantReadWriteLock;

public final class GcgK2ThreadLocalGetProbe {
    static final ThreadLocal<long[]> TL = ThreadLocal.withInitial(() -> new long[1]);
    static final ReentrantReadWriteLock RW = new ReentrantReadWriteLock();
    static volatile Object sink;

    public static void main(String[] a) throws Exception {
        final int threads = Integer.parseInt(a[0]);
        final int iters = Integer.parseInt(a[1]);
        final AtomicLong sum = new AtomicLong();
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < iters; i++) {
                    long[] cell = TL.get();
                    RW.readLock().lock();
                    try {
                        cell[0] += id + 1;
                    } finally {
                        RW.readLock().unlock();
                    }
                    if ((i & 1023) == 0) {
                        sink = new byte[2048];
                    }
                }
                sum.addAndGet(TL.get()[0]);
            }, "k2-tl-" + t);
        }
        long t0 = System.nanoTime();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("K2TLGET_OK threads=" + threads + " iters=" + iters + " sum=" + sum.get());
        System.err.println("[k2-tlget] ms=" + ms);
    }
}
