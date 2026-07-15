package cratonvm;

import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Exercises the same fresh-worker path used by Tomcat endpoint executors.
 *
 * <p>Each iteration constructs a real JDK ThreadPoolExecutor, creates a named
 * platform Thread through its factory, and immediately prestarts its only core
 * worker. A fresh Thread must always be startable: any IllegalThreadStateException
 * is a VM regression in Thread construction/state publication.</p>
 */
public final class ThreadPoolExecutorPrestartProbe {
    private static final int DEFAULT_ITERATIONS = 10_000;

    private ThreadPoolExecutorPrestartProbe() {
    }

    /** Mirrors Tomcat's TaskThread shape: a Thread subclass with its own task. */
    private static final class TomcatStyleTaskThread extends Thread {
        private final Runnable task;

        TomcatStyleTaskThread(Runnable task, String name) {
            super(name);
            this.task = task;
        }

        @Override
        public void run() {
            task.run();
        }
    }

    public static void main(String[] args) throws Exception {
        final int iterations = args.length == 0
                ? DEFAULT_ITERATIONS
                : Integer.parseInt(args[0]);
        final AtomicInteger created = new AtomicInteger();
        int started = 0;

        for (int i = 0; i < iterations; i++) {
            final ThreadFactory factory = runnable ->
                    new TomcatStyleTaskThread(
                            runnable, "tomcat-endpoint-prestart-" + created.incrementAndGet());
            final ThreadPoolExecutor executor = new ThreadPoolExecutor(
                    1, 1, 0L, TimeUnit.MILLISECONDS,
                    new LinkedBlockingQueue<Runnable>(), factory);
            try {
                if (executor.prestartAllCoreThreads() != 1) {
                    throw new AssertionError("expected exactly one prestarted worker at iteration " + i);
                }
                started++;
            } finally {
                executor.shutdown();
                if (!executor.awaitTermination(30, TimeUnit.SECONDS)) {
                    throw new AssertionError("worker did not terminate at iteration " + i);
                }
            }
        }

        if (created.get() != iterations || started != iterations) {
            throw new AssertionError("created=" + created.get() + " started=" + started);
        }
        System.out.println("PRESTART_OK iterations=" + iterations + " workers=" + started);
    }
}
