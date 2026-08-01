package org.apache.tomcat.util.threads;

import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The OTHER half of the WsRemoteEndpoint close-delay chain, measured directly.
 *
 * ForceProbe covers TaskQueue.force()'s rejection. This one covers why Tomcat's
 * connector pool grew from 10 threads to maxThreads=200 under CratonVM while
 * HotSpot's stayed at 10 -- which the known-issue doc originally attributed to a
 * ~10x socket-task amplification, i.e. to load. It is not load. TaskQueue.offer
 * asks the pool how big it is:
 *
 *     if (parent.getPoolSizeNoLock() == parent.getMaximumPoolSize()) return super.offer(o);
 *     if (parent.getSubmittedCount() <= parent.getPoolSizeNoLock())  return super.offer(o);
 *     if (parent.getPoolSizeNoLock() <  parent.getMaximumPoolSize()) return false;  // grow
 *
 * and getPoolSizeNoLock() is
 *
 *     return runStateAtLeast(ctl.get(), TIDYING) ? 0 : workers.size();
 *
 * -- f(g(), k) with a two-argument callee comparing its arguments, over a
 * concurrently mutated ctl. That is exactly the shape broken by the direct-call
 * argument clobber (jit/src/x64.rs, fixed 5fa4cdb6fb): the callee received arg0
 * in both slots, so `c >= s` evaluated `c >= c` = true and the method answered
 * "this pool has 0 threads". A 0 makes offer() take the grow branch on every
 * single submission, so the pool runs to maxThreads regardless of task volume.
 *
 * getPoolSize() computes the SAME expression under mainLock and is called rarely
 * (so it stays interpreted). The two disagreeing on one pool is the whole
 * defect, visible from Java with no host-load dependence.
 *
 * Lives in org.apache.tomcat.util.threads because getPoolSizeNoLock() is
 * protected. Needs the Tomcat jar (or classes dir) on the classpath.
 *
 * usage: PoolSizeNoLockProbe [readerThreads] [seconds]
 * exit 3 if any wrong answer was observed.
 */
public class PoolSizeNoLockProbe {

    static final AtomicLong calls = new AtomicLong();
    static final AtomicLong zeroWhileNonEmpty = new AtomicLong();
    static final AtomicLong disagree = new AtomicLong();
    static volatile boolean stop = false;

    /** Exposes the protected accessor without changing what it does. */
    static final class Peek extends ThreadPoolExecutor {
        Peek(TaskQueue q) {
            super(10, 200, 50, TimeUnit.MILLISECONDS, q, r -> {
                Thread t = new Thread(r);
                t.setDaemon(true);
                return t;
            });
        }

        int noLock() {
            return getPoolSizeNoLock();
        }
    }

    public static void main(String[] args) throws Exception {
        int readers = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int secs = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        TaskQueue q = new TaskQueue();
        Peek ex = new Peek(q);
        q.setParent(ex);
        // Let workers come and go so `ctl` is genuinely mutated while the
        // readers run. With a constant source the inliner takes the call and
        // no raw direct edge is emitted, so the defect cannot appear.
        ex.allowCoreThreadTimeOut(true);
        ex.prestartAllCoreThreads();

        Thread[] churn = new Thread[4];
        for (int i = 0; i < churn.length; i++) {
            churn[i] = new Thread(() -> {
                while (!stop) {
                    ex.execute(() -> { });
                    ex.prestartCoreThread();
                }
            });
            churn[i].setDaemon(true);
            churn[i].start();
        }

        Thread[] ts = new Thread[readers];
        for (int i = 0; i < readers; i++) {
            ts[i] = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 1000; k++) {
                        int n = ex.noLock();
                        calls.incrementAndGet();
                        if (n == 0) {
                            // The locked accessor evaluates the same
                            // expression; a running pool with live workers
                            // cannot have 0 threads.
                            if (ex.getPoolSize() > 0) {
                                zeroWhileNonEmpty.incrementAndGet();
                                disagree.incrementAndGet();
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
        System.out.println("getPoolSizeNoLock() calls=" + calls.get()
                + "  answered 0 for a non-empty pool=" + zeroWhileNonEmpty.get()
                + "  locked getPoolSize()=" + ex.getPoolSize()
                + "  max=" + ex.getMaximumPoolSize());
        ex.shutdownNow();
        System.exit(disagree.get() > 0 ? 3 : 0);
    }
}
