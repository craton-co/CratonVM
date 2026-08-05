import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Does an UNCONTENDED `ReentrantLock.lock()/unlock()` get more expensive as the
 * VM gains threads that are not touching the lock?
 *
 * On any correct JVM the answer is no: an uncontended acquire is a CAS plus a
 * field store, and the number of *other* live threads is irrelevant. This probe
 * exists because on CratonVM it was not irrelevant. `AbstractOwnableSynchronizer
 * .setExclusiveOwnerThread` is intercepted by a VM native so `ThreadMXBean` can
 * keep an owned-synchronizer index, and the interception used to update that
 * index by walking EVERY registered thread and taking EVERY thread's
 * per-thread mutex — twice per lock/unlock pair, since AQS calls the setter on
 * acquire (owner) and on release (null).
 *
 * So the cost of one uncontended lock scaled with the size of the thread pool
 * around it — invisible in a single-threaded microbenchmark (which is exactly
 * what `AqsBreakdownProbe` is) and quadratic-ish in a server workload.
 *
 * The control rung is a `synchronized` block: a monitor is one bytecode the VM
 * implements directly, it never reaches `setExclusiveOwnerThread`, and it must
 * stay flat across thread counts on both VMs. If BOTH rungs slope, the host is
 * loaded and the run is void — rerun it.
 *
 * Read the RATIO down each column, not the absolute numbers.
 *
 *   javac -d out probes/AqsOwnerScaleProbe.java
 *   java  -cp out AqsOwnerScaleProbe          # HotSpot control: flat
 *   cratonvm --java-home $jdk -cp out AqsOwnerScaleProbe
 */
public final class AqsOwnerScaleProbe {

    // Up to 256: the per-lock cost of a Θ(threads) walk only rises above this
    // measurement's noise once the walk is hundreds of mutex acquisitions long.
    // At 8 threads it is ~0.2 us against a ~23 us pair — invisible, which is
    // exactly why reading the code found this and the first probe did not.
    private static final int[] THREAD_COUNTS = { 1, 16, 64, 256 };
    private static final int PASSES = 3;
    private static final int ROUNDS = 200_000;

    private static final ReentrantLock LOCK = new ReentrantLock();
    private static final Object MONITOR = new Object();

    private static long sink;

    /** Uncontended AQS acquire/release. Two `setExclusiveOwnerThread` calls. */
    private static long rLock(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            LOCK.lock();
            a += i;
            LOCK.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    /** Uncontended monitor. Never reaches AQS — the flat control. */
    private static long rSync(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            synchronized (MONITOR) {
                a += i;
            }
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    /**
     * Park `extra` threads and keep them alive (and registered) for the caller.
     * They do no work at all — the point is purely that the VM knows about them.
     */
    private static Thread[] spawnIdle(int extra, CountDownLatch stop) throws Exception {
        Thread[] ts = new Thread[extra];
        CountDownLatch up = new CountDownLatch(extra);
        for (int i = 0; i < extra; i++) {
            ts[i] = new Thread(() -> {
                up.countDown();
                try {
                    stop.await();
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            }, "idle-" + i);
            ts[i].setDaemon(true);
            ts[i].start();
        }
        up.await();
        return ts;
    }

    public static void main(String[] args) throws Exception {
        // Warm both rungs before the first measured row so the table is not
        // reading compilation.
        rLock(ROUNDS);
        rSync(ROUNDS);

        System.out.printf("%-14s %14s %14s%n", "live threads", "lock/unlock", "synchronized");
        int prevThreads = 0;
        for (int want : THREAD_COUNTS) {
            CountDownLatch stop = new CountDownLatch(1);
            Thread[] idle = spawnIdle(want - prevThreads - (prevThreads == 0 ? 1 : 0), stop);
            prevThreads = want;

            double bestLock = Double.MAX_VALUE;
            double bestSync = Double.MAX_VALUE;
            for (int p = 0; p < PASSES; p++) {
                bestLock = Math.min(bestLock, rLock(ROUNDS) / (double) ROUNDS);
                bestSync = Math.min(bestSync, rSync(ROUNDS) / (double) ROUNDS);
            }
            System.out.printf("%-14d %14.1f %14.1f%n", want, bestLock, bestSync);

            // Leave them parked: the next row wants MORE threads, not fresh
            // ones, so releasing here would undo the measurement. `stop` is
            // held until the JVM exits (the threads are daemons).
            if (idle.length < 0) {
                LockSupport.parkNanos(1);
            }
        }
        if (sink == Long.MIN_VALUE) {
            System.out.println("unreachable");
        }
    }
}
