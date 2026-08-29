import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

/**
 * Wakeup cost of timed parks: N threads each parking 50 ms in a loop for a
 * fixed wall window. Reports total completed parks and the mean park, so a
 * change in the park machinery shows up as both a cadence change (mean) and a
 * throughput one (count). Process CPU time is measured by the caller.
 */
public final class ParkCostProbe {
    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 16;
        long windowMs = args.length > 1 ? Long.parseLong(args[1]) : 6000;
        final long parkNanos = TimeUnit.MILLISECONDS.toNanos(50);

        final CountDownLatch ready = new CountDownLatch(threads);
        final CountDownLatch go = new CountDownLatch(1);
        final long[] counts = new long[threads];
        final long[] totalNanos = new long[threads];
        Thread[] ts = new Thread[threads];
        final long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(windowMs);

        for (int i = 0; i < threads; i++) {
            final int id = i;
            ts[i] = new Thread(() -> {
                ready.countDown();
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                long n = 0, sum = 0;
                while (System.nanoTime() < deadline) {
                    long t0 = System.nanoTime();
                    LockSupport.parkNanos(parkNanos);
                    sum += System.nanoTime() - t0;
                    n++;
                }
                counts[id] = n;
                totalNanos[id] = sum;
            }, "parker-" + i);
            ts[i].setDaemon(true);
            ts[i].start();
        }
        ready.await();
        go.countDown();
        for (Thread t : ts) {
            t.join();
        }
        long n = 0, sum = 0;
        for (int i = 0; i < threads; i++) {
            n += counts[i];
            sum += totalNanos[i];
        }
        System.out.printf("threads=%d window=%dms parks=%d meanPark=%.3fms%n",
                          threads, windowMs, n, n == 0 ? 0.0 : (sum / (double) n) / 1e6);
        System.exit(0);
    }
}
