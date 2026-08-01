import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

import org.apache.tomcat.util.threads.TaskQueue;
import org.apache.tomcat.util.threads.ThreadPoolExecutor;

/**
 * Direct, load-independent A/B of the step that fails in
 * TestWsRemoteEndpointImplServerDeadlock.
 *
 * TaskQueue.force() is public and its ONLY failure mode is
 *     if (parent == null || parent.isShutdown())
 *         throw new RejectedExecutionException("Executor not running, ...");
 * The executor here is never shut down, so every rejection is a wrong answer
 * from isShutdown() -- i.e. from `runStateAtLeast(ctl.get(), SHUTDOWN)`, which
 * is the call shape broken by the direct-call argument clobber.
 *
 * Unlike the end-to-end test this needs no pool-size race and no host load.
 *
 * usage: ForceProbe <threads> <seconds>
 */
public class ForceProbe {

    static final AtomicLong calls = new AtomicLong();
    static final AtomicLong notRunning = new AtomicLong();
    static final AtomicLong otherRej = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int nThreads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int secs = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        TaskQueue q = new TaskQueue();
        ThreadPoolExecutor ex = new ThreadPoolExecutor(10, 200, 60, TimeUnit.SECONDS, q, r -> {
            Thread t = new Thread(r);
            t.setDaemon(true);
            return t;
        });
        q.setParent(ex);

        Runnable noop = () -> { };

        Thread[] ts = new Thread[nThreads];
        for (int i = 0; i < nThreads; i++) {
            ts[i] = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 1000; k++) {
                        calls.incrementAndGet();
                        try {
                            q.force(noop);
                        } catch (RejectedExecutionException e) {
                            if (String.valueOf(e.getMessage()).contains("not running")) {
                                notRunning.incrementAndGet();
                            } else {
                                otherRej.incrementAndGet();
                            }
                        }
                    }
                }
            });
            ts[i].setDaemon(true);
            ts[i].start();
        }

        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);
        System.out.println("force() calls=" + calls.get()
                + "  notRunning=" + notRunning.get()
                + "  otherRej=" + otherRej.get()
                + "  isShutdown=" + ex.isShutdown()
                + "  poolSize=" + ex.getPoolSize());
        ex.shutdownNow();
        System.exit(notRunning.get() > 0 ? 3 : 0);
    }
}
