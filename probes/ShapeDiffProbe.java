import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

import org.apache.tomcat.util.threads.TaskQueue;
import org.apache.tomcat.util.threads.ThreadPoolExecutor;

/**
 * Differential probe: the same predicate written two ways, evaluated back to back
 * on the same thread, under the load that makes it fail.
 *
 *   A. viaFn     = isShutdownShape()                     -- Tomcat's exact shape:
 *                    private static boolean isShutdownShape() {
 *                        return runStateAtLeast(ctl.get(), SHUTDOWN);
 *                    }
 *   B. viaInline = runStateAtLeast(ctl.get(), SHUTDOWN)  -- same call, inlined at
 *                                                           the call site by hand
 *
 * ctl only ever holds RUNNING..RUNNING+200, all negative, so BOTH must always be
 * false. Any true is a wrong answer; any A != B is a miscompilation of one form.
 *
 * The Tomcat executor is here only to reproduce the load (200 pool threads, heavy
 * allocation) under which the wrong answer appears.
 *
 * usage: ShapeDiffProbe <submitterThreads> <seconds> [readerThreads]
 */
public class ShapeDiffProbe {

    static final int COUNT_BITS = Integer.SIZE - 3;
    static final int RUNNING = -1 << COUNT_BITS;
    static final int SHUTDOWN = 0;
    static final AtomicInteger ctl = new AtomicInteger(RUNNING);

    private static boolean runStateAtLeast(int c, int s) {
        return c >= s;
    }

    private static boolean isShutdownShape() {
        return runStateAtLeast(ctl.get(), SHUTDOWN);
    }

    static final AtomicLong calls = new AtomicLong();
    static final AtomicLong wrongFn = new AtomicLong();
    static final AtomicLong wrongInline = new AtomicLong();
    static final AtomicLong disagree = new AtomicLong();
    static final AtomicLong reported = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int nSub = args.length > 0 ? Integer.parseInt(args[0]) : 24;
        int secs = args.length > 1 ? Integer.parseInt(args[1]) : 15;
        int nRead = args.length > 2 ? Integer.parseInt(args[2]) : 4;

        TaskQueue q = new TaskQueue();
        ThreadPoolExecutor ex = new ThreadPoolExecutor(10, 200, 60, TimeUnit.SECONDS, q, r -> {
            Thread t = new Thread(r);
            t.setDaemon(true);
            return t;
        });
        q.setParent(ex);
        Runnable task = () -> {
            long x = 0;
            for (int i = 0; i < 200; i++) {
                x += i;
            }
            if (x == -1) {
                System.out.print("");
            }
        };
        for (int i = 0; i < nSub; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    try {
                        ex.execute(task);
                    } catch (RuntimeException e) {
                        // counted elsewhere; load generator only
                    }
                }
            });
            t.setDaemon(true);
            t.start();
        }

        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        ctl.incrementAndGet();
                    }
                    for (int k = 0; k < 200; k++) {
                        ctl.decrementAndGet();
                    }
                }
            }, "mut-" + i);
            t.setDaemon(true);
            t.start();
        }

        for (int i = 0; i < nRead; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 5000; k++) {
                        boolean viaFn = isShutdownShape();
                        int c = ctl.get();
                        boolean viaInline = runStateAtLeast(c, SHUTDOWN);
                        if (viaFn) {
                            wrongFn.incrementAndGet();
                        }
                        if (viaInline) {
                            wrongInline.incrementAndGet();
                        }
                        if (viaFn != viaInline) {
                            disagree.incrementAndGet();
                            if (reported.getAndIncrement() < 6) {
                                System.out.println("DISAGREE viaFn=" + viaFn + " viaInline=" + viaInline
                                        + " c=" + c + " (0x" + Integer.toHexString(c) + ")"
                                        + " ctlNow=" + ctl.get()
                                        + " fnAgain=" + isShutdownShape()
                                        + " helperOnC=" + runStateAtLeast(c, SHUTDOWN)
                                        + " directOnC=" + (c >= 0)
                                        + " thread=" + Thread.currentThread().getName());
                                System.out.flush();
                            }
                        }
                    }
                    calls.addAndGet(5000);
                }
            }, "read-" + i);
            t.setDaemon(true);
            t.start();
        }

        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);
        System.out.println("calls=" + calls.get()
                + " wrongFn=" + wrongFn.get()
                + " wrongInline=" + wrongInline.get()
                + " disagree=" + disagree.get()
                + " ctlNow=" + ctl.get()
                + " poolSize=" + ex.getPoolSize()
                + " exIsShutdown=" + ex.isShutdown());
        ex.shutdownNow();
        System.exit(wrongFn.get() > 0 || wrongInline.get() > 0 ? 3 : 0);
    }
}
