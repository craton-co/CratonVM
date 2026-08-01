import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Minimal repro for the wrong answer behind
 * "RejectedExecutionException: Executor not running" in Tomcat's TaskQueue.
 *
 * Tomcat's ThreadPoolExecutor.isShutdown() is exactly:
 *     runStateAtLeast(ctl.get(), SHUTDOWN)   with   private static boolean
 *     runStateAtLeast(int c, int s) { return c >= s; }   and   SHUTDOWN == 0
 * and ctl always holds RUNNING|workerCount, i.e. a NEGATIVE int while the pool runs.
 *
 * Each reader reports, for any call that answered "shut down":
 *   c           the exact int that was compared
 *   helper      c >= 0 through the private static helper (what Tomcat calls)
 *   direct      c >= 0 written inline at the call site
 *   helperAgain the same helper call repeated on the same c
 *   local       c >= 0 where c came from a plain local, no AtomicInteger
 *
 * usage: IntCmpProbe <readerThreads> <mutatorThreads> <seconds>
 */
public class IntCmpProbe {

    static final int COUNT_BITS = Integer.SIZE - 3;
    static final int RUNNING = -1 << COUNT_BITS;
    static final int SHUTDOWN = 0;

    static final AtomicInteger ctl = new AtomicInteger(RUNNING);

    private static boolean runStateAtLeast(int c, int s) {
        return c >= s;
    }

    static final AtomicLong calls = new AtomicLong();
    static final AtomicLong wrong = new AtomicLong();
    static final AtomicLong reported = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int nRead = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int nMut = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int secs = args.length > 2 ? Integer.parseInt(args[2]) : 10;

        Thread[] muts = new Thread[nMut];
        for (int i = 0; i < nMut; i++) {
            muts[i] = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        ctl.incrementAndGet();
                    }
                    for (int k = 0; k < 200; k++) {
                        ctl.decrementAndGet();
                    }
                }
            }, "mut-" + i);
            muts[i].setDaemon(true);
        }

        Thread[] reads = new Thread[nRead];
        for (int i = 0; i < nRead; i++) {
            reads[i] = new Thread(() -> {
                long n = 0;
                while (!stop) {
                    for (int k = 0; k < 10000; k++) {
                        n++;
                        int c = ctl.get();
                        if (runStateAtLeast(c, SHUTDOWN)) {
                            wrong.incrementAndGet();
                            if (reported.getAndIncrement() < 6) {
                                boolean direct = c >= 0;
                                boolean again = runStateAtLeast(c, SHUTDOWN);
                                int cc = c;
                                boolean local = cc >= 0;
                                System.out.println("WRONG iter=" + n + " c=" + c
                                        + " (0x" + Integer.toHexString(c) + ")"
                                        + " helper=" + runStateAtLeast(c, SHUTDOWN)
                                        + " direct=" + direct
                                        + " helperAgain=" + again
                                        + " local=" + local
                                        + " lt=" + (c < 0)
                                        + " eq0=" + (c == 0)
                                        + " ctlNow=" + ctl.get()
                                        + " RUNNING=" + RUNNING
                                        + " thread=" + Thread.currentThread().getName());
                                System.out.flush();
                            }
                        }
                    }
                    calls.addAndGet(10000);
                }
            }, "read-" + i);
            reads[i].setDaemon(true);
        }

        for (Thread t : muts) {
            t.start();
        }
        for (Thread t : reads) {
            t.start();
        }
        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);

        System.out.println("calls=" + calls.get() + " wrong=" + wrong.get()
                + " ctlNow=" + ctl.get() + " RUNNING=" + RUNNING
                + " sanityHelper=" + runStateAtLeast(RUNNING, SHUTDOWN)
                + " sanityDirect=" + (RUNNING >= 0));
        System.exit(wrong.get() > 0 ? 3 : 0);
    }
}
