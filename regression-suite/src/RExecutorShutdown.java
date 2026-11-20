import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Regression: BUG-EXEC-SHUTDOWN-INTERRUPTS-RUNNING-TASK-20260726.
 *
 * `ExecutorService.shutdown()` is an ORDERLY shutdown: "previously submitted
 * tasks are executed" and no running task is interrupted. Only IDLE workers are
 * interrupted, so they can leave `getTask()`; the JDK distinguishes the two with
 * `w.tryLock()` in `interruptIdleWorkers`. CratonVM's shutdown bridge used
 * `shutdownNow()`'s "interrupt every worker" instead, so a task that was mid-run
 * observed a spurious interrupt.
 *
 * That is not cosmetic: H2's `MVStore.close()` shuts its buffer-save pool down
 * while a worker is inside `FileChannel.write`, and an interrupt there aborts
 * the write through `AbstractInterruptibleChannel.begin()` — surfacing as
 * `MVStoreException` and a failed `TestStreamStore`.
 *
 * `shutdownNow()` must keep interrupting running tasks; that half is asserted
 * too, so a fix for one cannot silently break the other.
 */
public class RExecutorShutdown {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** shutdown(): the running task must finish, uninterrupted. */
    static void orderly() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(2);
        CountDownLatch started = new CountDownLatch(1);
        AtomicBoolean sawInterrupt = new AtomicBoolean(false);
        AtomicBoolean completed = new AtomicBoolean(false);
        Future<?> f = ex.submit(() -> {
            started.countDown();
            long end = System.currentTimeMillis() + 1500;
            while (System.currentTimeMillis() < end) {
                if (Thread.currentThread().isInterrupted()) {
                    sawInterrupt.set(true);
                    return;
                }
            }
            completed.set(true);
        });
        check(started.await(30, TimeUnit.SECONDS), "task never started");
        ex.shutdown();
        f.get(60, TimeUnit.SECONDS);
        check(ex.awaitTermination(60, TimeUnit.SECONDS), "shutdown() did not terminate the pool");
        check(!sawInterrupt.get(), "shutdown() interrupted a RUNNING task");
        check(completed.get(), "the running task did not run to completion across shutdown()");
    }

    /** shutdown(): a pool whose workers are all idle must still terminate. */
    static void idleWorkersWake() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(3);
        AtomicInteger ran = new AtomicInteger();
        for (int i = 0; i < 3; i++) {
            ex.submit(ran::incrementAndGet);
        }
        // Let all three finish and park in getTask().
        Thread.sleep(300);
        ex.shutdown();
        check(ex.awaitTermination(60, TimeUnit.SECONDS),
                "shutdown() left idle workers parked (awaitTermination timed out)");
        check(ran.get() == 3, "not every submitted task ran: " + ran.get());
    }

    /** shutdownNow(): a running task MUST be interrupted. */
    static void forceful() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(1);
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch interrupted = new CountDownLatch(1);
        ex.submit(() -> {
            started.countDown();
            long end = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < end) {
                if (Thread.currentThread().isInterrupted()) {
                    interrupted.countDown();
                    return;
                }
            }
        });
        check(started.await(30, TimeUnit.SECONDS), "task never started");
        ex.shutdownNow();
        check(interrupted.await(30, TimeUnit.SECONDS),
                "shutdownNow() did NOT interrupt the running task");
        ex.awaitTermination(30, TimeUnit.SECONDS);
    }

    public static void main(String[] args) throws Exception {
        orderly();
        idleWorkersWake();
        forceful();
        System.out.println("CK RExecutorShutdown checks=" + checks);
        System.out.println("PASS RExecutorShutdown");
    }
}
