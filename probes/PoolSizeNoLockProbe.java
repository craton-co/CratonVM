package org.apache.tomcat.util.threads;

import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The OTHER half of the WsRemoteEndpoint close-delay chain, measured directly.
 *
 * ForceProbe covers TaskQueue.force()'s rejection. This one covers why Tomcat's
 * connector pool ran from 10 threads to maxThreads under CratonVM while
 * HotSpot's stayed at 10 -- which the known-issue doc originally attributed to a
 * ~10x socket-task amplification, i.e. to load. It is not load. TaskQueue.offer
 * sizes the pool with ThreadPoolExecutor.getPoolSizeNoLock():
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
 * "this pool has 0 threads". A 0 sends offer() down its grow branch on every
 * submission, so the pool runs to maxThreads regardless of task volume.
 *
 * ORACLES. Two, because the first one turned out not to discriminate:
 *
 *   1. `offer()` at poolSize == maxPoolSize must queue and return true -- its
 *      first branch is unconditional there. MEASURED INERT: when
 *      getPoolSizeNoLock() answers 0 the SECOND branch,
 *      `getSubmittedCount() <= 0`, is frequently true as well (the short feeder
 *      tasks drain submittedCount), so offer() returns true anyway and the
 *      wrong answer never reaches the return value. Kept because it is the
 *      production path, but a 0 here is not an elimination.
 *
 *   2. `getPoolSizeNoLock()` answering 0 while the locked `getPoolSize()` is
 *      non-zero. Both evaluate the SAME expression; the locked one is called
 *      rarely enough to stay interpreted, so it is a control rather than a
 *      second sample. This is the discriminator, measured 2026-08-01:
 *
 *          HotSpot             12,435,934 calls     0 wrong
 *          CratonVM before            338 calls   233 wrong  (69%)
 *          CratonVM after             478 calls     0 wrong
 *
 * The pool is kept churning (1 ms keep-alive, core threads allowed to time out,
 * a steady stream of short tasks) because `ctl` must be genuinely mutated: with
 * a constant source the inliner takes the call, no raw direct edge is emitted,
 * and the defect cannot appear.
 *
 * Lives in org.apache.tomcat.util.threads because getPoolSizeNoLock() is
 * protected. Needs the Tomcat jar (or classes dir) on the classpath.
 *
 * usage: PoolSizeNoLockProbe [readerThreads] [seconds]
 * exit 3 if any wrong answer was observed.
 */
public class PoolSizeNoLockProbe {

    static final int MAX = 12;

    static final AtomicLong offersAtMax = new AtomicLong();
    static final AtomicLong offerSaidGrow = new AtomicLong();
    static final AtomicLong noLockCalls = new AtomicLong();
    static final AtomicLong noLockSaidZero = new AtomicLong();
    static volatile boolean stop = false;

    /** Exposes the protected accessor without changing what it does. */
    static final class Peek extends ThreadPoolExecutor {
        Peek(TaskQueue q) {
            super(10, MAX, 1, TimeUnit.MILLISECONDS, q, r -> {
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
        ex.allowCoreThreadTimeOut(true);
        ex.prestartAllCoreThreads();

        // Keep the pool loaded enough to sit at its maximum most of the time,
        // and churning enough that `ctl` never looks constant to the compiler.
        Thread feeder = new Thread(() -> {
            while (!stop) {
                try {
                    ex.execute(() -> {
                        try {
                            Thread.sleep(2);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                        }
                    });
                } catch (RuntimeException e) {
                    // saturated; back off and keep going
                }
                if (ex.getPoolSize() >= MAX) {
                    try {
                        Thread.sleep(1);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "feeder");
        feeder.setDaemon(true);
        feeder.start();

        final Runnable noop = () -> { };

        Thread[] ts = new Thread[readers];
        for (int i = 0; i < readers; i++) {
            ts[i] = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        // Oracle 1 -- the production path.
                        if (ex.getPoolSize() == ex.getMaximumPoolSize()) {
                            boolean queued = q.offer(noop);
                            offersAtMax.incrementAndGet();
                            if (!queued && ex.getPoolSize() == ex.getMaximumPoolSize()) {
                                // A pool that is at its maximum has nowhere to
                                // grow; offer() must have queued this.
                                offerSaidGrow.incrementAndGet();
                            }
                            if (queued) {
                                q.remove(noop);
                            }
                            // Oracle 2 -- the accessor itself, read directly.
                            // Weaker (this call shape is easy to inline, which
                            // removes the raw direct edge) but unambiguous.
                            noLockCalls.incrementAndGet();
                            if (ex.noLock() == 0 && ex.getPoolSize() > 0) {
                                noLockSaidZero.incrementAndGet();
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
        System.out.println("offer() at poolSize==max: calls=" + offersAtMax.get()
                + "  wrongly refused (said grow)=" + offerSaidGrow.get());
        System.out.println("getPoolSizeNoLock():      calls=" + noLockCalls.get()
                + "  answered 0 for a live pool=" + noLockSaidZero.get());
        System.out.println("final locked getPoolSize()=" + ex.getPoolSize()
                + " max=" + ex.getMaximumPoolSize());
        ex.shutdownNow();
        System.exit(offerSaidGrow.get() + noLockSaidZero.get() > 0 ? 3 : 0);
    }
}
