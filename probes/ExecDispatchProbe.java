import java.util.Arrays;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.Semaphore;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * The primitive Tomcat's WebSocket completion path actually uses.
 *
 * `WsRemoteEndpointImplServer.clearHandler(null, true)` does
 * `socketWrapper.execute(new OnResultRunnable(...))` — i.e. it hands the
 * SendHandler callback to the container's ThreadPoolExecutor rather than
 * calling it inline. Only after that runnable runs on a pool thread does
 * `semaphore.release()` happen, which is what unblocks the endpoint thread to
 * issue the next message. So the SEQ2 gap contains a full
 * `execute() -> task starts` dispatch that the doc's Semaphore /
 * LockSupport / Object.wait measurements never covered.
 *
 * Measures, over N rounds on a warm pool:
 *   A. execute() -> task body entered            (dispatch alone)
 *   B. execute() -> semaphore.release() -> acquire() returns on the caller
 *      (the whole clearHandler -> next-send handoff)
 */
public class ExecDispatchProbe {

    private static final int WARMUP = 2000;
    private static final int ROUNDS = 20000;

    private static void report(String label, long[] ns, int n) {
        long[] s = Arrays.copyOf(ns, n);
        Arrays.sort(s);
        System.out.printf("%-46s median %6.1f us   p90 %7.1f us   p99 %8.1f us   max %9.1f us%n",
                label, s[n / 2] / 1000.0, s[(int) (n * 0.90)] / 1000.0,
                s[(int) (n * 0.99)] / 1000.0, s[n - 1] / 1000.0);
    }

    private static void run(String poolLabel, ThreadPoolExecutor ex) throws Exception {
        long[] dispatch = new long[ROUNDS];
        long[] roundTrip = new long[ROUNDS];
        final long[] started = new long[1];
        Semaphore sem = new Semaphore(0);

        for (int i = 0; i < WARMUP + ROUNDS; i++) {
            final int idx = i - WARMUP;
            long t0 = System.nanoTime();
            ex.execute(() -> {
                started[0] = System.nanoTime();
                sem.release();
            });
            sem.acquire();
            long t2 = System.nanoTime();
            if (idx >= 0) {
                dispatch[idx] = started[0] - t0;
                roundTrip[idx] = t2 - t0;
            }
        }
        report(poolLabel + " execute() -> task entered", dispatch, ROUNDS);
        report(poolLabel + " execute() -> release() -> acquire()", roundTrip, ROUNDS);
        ex.shutdownNow();
    }

    public static void main(String[] args) throws Exception {
        // Tomcat's container pool: bounded queue, core threads pre-started.
        ThreadPoolExecutor tomcatLike = new ThreadPoolExecutor(10, 10, 60, TimeUnit.SECONDS,
                new LinkedBlockingQueue<>());
        tomcatLike.prestartAllCoreThreads();
        run("LinkedBlockingQueue(10 core)", tomcatLike);

        // One core thread, same queue: isolates "wake the one parked worker"
        // from any multi-worker steering effect.
        ThreadPoolExecutor single = new ThreadPoolExecutor(1, 1, 60, TimeUnit.SECONDS,
                new LinkedBlockingQueue<>());
        single.prestartAllCoreThreads();
        run("LinkedBlockingQueue(1 core)", single);
    }
}
