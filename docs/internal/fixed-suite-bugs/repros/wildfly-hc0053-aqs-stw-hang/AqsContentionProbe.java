import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.TimeUnit;

/**
 * Mirrors the exact contention pattern seen live in the WFLYHC0053 STW-census
 * capture: ~28 threads all hammering ONE shared ReentrantLock (mimicking
 * ConcreteResourceRegistration's internal lock under WildFly's
 * ParallelExtensionAddHandler), with concurrent forced-GC pressure, to test
 * whether AbstractQueuedSynchronizer's internal LockSupport.park()/
 * unpark(node.waiter) signaling ever permanently strands a waiter (the same
 * "stale post-GC-move Thread mirror" class of bug already tested cleanly
 * against EnhancedQueueExecutor directly, now targeting AQS's own queued-node
 * wakeup instead).
 */
public class AqsContentionProbe {
    static final ReentrantLock LOCK = new ReentrantLock();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int threadCount = 28;
        int roundsPerThread = 4000;
        AtomicInteger totalAcquires = new AtomicInteger(0);
        CountDownLatch done = new CountDownLatch(threadCount);

        // GC-pressure thread, same pattern as EqeUnparkProbe.
        Thread gcPressure = new Thread(() -> {
            java.util.List<Object> garbage = new java.util.ArrayList<>();
            long i = 0;
            while (!stop) {
                garbage.add(new byte[256]);
                if (garbage.size() > 2000) garbage.clear();
                if ((i++ % 300) == 0) {
                    System.gc();
                }
            }
        });
        gcPressure.setDaemon(true);
        gcPressure.start();

        Thread[] workers = new Thread[threadCount];
        for (int t = 0; t < threadCount; t++) {
            final int idx = t;
            workers[t] = new Thread(() -> {
                for (int r = 0; r < roundsPerThread; r++) {
                    LOCK.lock();
                    try {
                        totalAcquires.incrementAndGet();
                    } finally {
                        LOCK.unlock();
                    }
                }
                done.countDown();
            }, "worker-" + idx);
            workers[t].setDaemon(true);
        }
        long start = System.currentTimeMillis();
        for (Thread w : workers) w.start();

        boolean finished = done.await(60, TimeUnit.SECONDS);
        long elapsed = System.currentTimeMillis() - start;
        stop = true;

        if (!finished) {
            System.out.println("AQS_PROBE_RESULT=STUCK remainingLatch=" + done.getCount()
                    + " totalAcquires=" + totalAcquires.get()
                    + " expectedTotal=" + (threadCount * roundsPerThread)
                    + " elapsedMs=" + elapsed);
            // Dump thread states of any workers that never finished.
            for (Thread w : workers) {
                if (w.isAlive()) {
                    System.out.println("STUCK_THREAD " + w.getName() + " state=" + w.getState());
                    for (StackTraceElement f : w.getStackTrace()) {
                        System.out.println("    at " + f);
                    }
                }
            }
        } else {
            System.out.println("AQS_PROBE_RESULT=CLEAN totalAcquires=" + totalAcquires.get()
                    + " expectedTotal=" + (threadCount * roundsPerThread)
                    + " elapsedMs=" + elapsed);
        }
    }
}
