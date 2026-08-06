import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.Callable;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * L10 (jdk-only wave 2) — behavioural oracle for real `ThreadPoolExecutor`
 * field initialisation.
 *
 * The lane's claim is that `Executors.new*ThreadPool()` returns objects built
 * by the REAL `ThreadPoolExecutor.<init>`, so the eight receiver-shape dispatch
 * sites that exist to detect "this executor was not built by real bytecode"
 * become universally true and can be deleted (L11).
 *
 * A census row or a Rust-side predicate is not evidence of that. This probe is:
 * everything below goes through public API only, so the host JDK is the oracle,
 * and every line must print identically under `java`, `cratonvm --real-jdk` and
 * `cratonvm --jdk-only`.
 *
 * Three guards the evidence record specifically demands, because a probe that
 * only checks "the task ran" passes with all three broken:
 *
 *  * **async** — `execute()` must run the task on a worker thread, not inline
 *    on the caller. Asserted as "the executing thread is not the submitter",
 *    never as a timing measurement.
 *  * **self-recursion** — a task that calls `execute()` again from inside a
 *    worker must not recurse into the same native forever. That failure aborts
 *    the process rather than throwing, so it has to be executed, not reasoned
 *    about.
 *  * **cache poisoning** — the same call site must stay correct when several
 *    executor shapes alternate through it, which is what a monomorphic
 *    inline cache gets wrong.
 *
 * Nothing here prints a thread name, an address, a timestamp or an elapsed
 * time: those diverge from the oracle on every run and would make the probe
 * useless as a diff. Waits are bounded and their SUCCESS is printed, not their
 * duration.
 */
public final class L10ThreadPoolInitProbe {

    public static void main(String[] args) throws Exception {
        fixedPoolShape();
        cachedPoolShape();
        singleThreadShape();
        directConstructionMatchesFactory();
        realFieldsAreInitialised();
        asyncGuard();
        selfRecursionGuard();
        cachePoisoningGuard();
        lifecycle();
        threadFactoryIsHonoured();
        scheduledShape();
        System.out.println("L10ThreadPoolInitProbe done");
    }

    // ------------------------------------------------------------------
    // Shape: what each factory returns, and what the real <init> set up.
    // ------------------------------------------------------------------

    static void fixedPoolShape() {
        ExecutorService es = Executors.newFixedThreadPool(3);
        try {
            System.out.println("fixed.class=" + es.getClass().getName());
            ThreadPoolExecutor tpe = (ThreadPoolExecutor) es;
            System.out.println("fixed.core=" + tpe.getCorePoolSize()
                    + " max=" + tpe.getMaximumPoolSize()
                    + " keepAliveMs=" + tpe.getKeepAliveTime(TimeUnit.MILLISECONDS));
            System.out.println("fixed.queue=" + tpe.getQueue().getClass().getName()
                    + " size=" + tpe.getQueue().size()
                    + " remaining=" + tpe.getQueue().remainingCapacity());
            System.out.println("fixed.handler=" + tpe.getRejectedExecutionHandler().getClass().getName());
            System.out.println("fixed.factory=" + tpe.getThreadFactory().getClass().getName());
            System.out.println("fixed.allowsCoreThreadTimeOut=" + tpe.allowsCoreThreadTimeOut());
            System.out.println("fixed.atRest pool=" + tpe.getPoolSize()
                    + " active=" + tpe.getActiveCount()
                    + " tasks=" + tpe.getTaskCount()
                    + " completed=" + tpe.getCompletedTaskCount()
                    + " largest=" + tpe.getLargestPoolSize());
            System.out.println("fixed.state shutdown=" + tpe.isShutdown()
                    + " terminated=" + tpe.isTerminated()
                    + " terminating=" + tpe.isTerminating());
        } finally {
            es.shutdown();
        }
    }

    static void cachedPoolShape() {
        ExecutorService es = Executors.newCachedThreadPool();
        try {
            System.out.println("cached.class=" + es.getClass().getName());
            ThreadPoolExecutor tpe = (ThreadPoolExecutor) es;
            System.out.println("cached.core=" + tpe.getCorePoolSize()
                    + " max=" + tpe.getMaximumPoolSize()
                    + " keepAliveSec=" + tpe.getKeepAliveTime(TimeUnit.SECONDS));
            System.out.println("cached.queue=" + tpe.getQueue().getClass().getName()
                    + " remaining=" + tpe.getQueue().remainingCapacity());
            System.out.println("cached.handler=" + tpe.getRejectedExecutionHandler().getClass().getName());
        } finally {
            es.shutdown();
        }
    }

    /**
     * `newSingleThreadExecutor` is deliberately NOT a bare `ThreadPoolExecutor`
     * in the real JDK — it is wrapped so the pool cannot be reconfigured. A VM
     * that hands back a raw executor here diverges from the oracle on the very
     * first line, and every `instanceof ThreadPoolExecutor` a caller writes
     * flips.
     */
    static void singleThreadShape() {
        ExecutorService es = Executors.newSingleThreadExecutor();
        try {
            System.out.println("single.class=" + es.getClass().getName());
            System.out.println("single.isTpe=" + (es instanceof ThreadPoolExecutor));
            System.out.println("single.shutdown=" + es.isShutdown()
                    + " terminated=" + es.isTerminated());
        } finally {
            es.shutdown();
        }
    }

    /**
     * The whole point of the lane: a factory-made pool and a `new
     * ThreadPoolExecutor(...)` built by ordinary user bytecode must be the
     * same kind of object. Any divergence here IS the half-real construction
     * defect.
     */
    static void directConstructionMatchesFactory() {
        ThreadPoolExecutor direct = new ThreadPoolExecutor(
                3, 3, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
        ThreadPoolExecutor factory = (ThreadPoolExecutor) Executors.newFixedThreadPool(3);
        try {
            System.out.println("same.class=" + direct.getClass().getName().equals(factory.getClass().getName()));
            System.out.println("same.core=" + (direct.getCorePoolSize() == factory.getCorePoolSize()));
            System.out.println("same.max=" + (direct.getMaximumPoolSize() == factory.getMaximumPoolSize()));
            System.out.println("same.queueClass=" + direct.getQueue().getClass().getName()
                    .equals(factory.getQueue().getClass().getName()));
            System.out.println("same.handlerClass=" + direct.getRejectedExecutionHandler().getClass().getName()
                    .equals(factory.getRejectedExecutionHandler().getClass().getName()));
            System.out.println("same.keepAlive=" + (direct.getKeepAliveTime(TimeUnit.MILLISECONDS)
                    == factory.getKeepAliveTime(TimeUnit.MILLISECONDS)));
        } finally {
            direct.shutdown();
            factory.shutdown();
        }
    }

    /**
     * `ctl` is the known missing field — a synthetic executor NPEs on it
     * immediately. `mainLock`, `workers`, `workQueue` and `termination` are the
     * same family. None is public, so each is exercised through the public
     * method whose real bytecode dereferences it:
     *
     *   ctl        -> isShutdown / isTerminating / getActiveCount
     *   mainLock   -> getPoolSize / getTaskCount / getLargestPoolSize
     *   workers    -> getPoolSize after a task has run
     *   workQueue  -> getQueue().offer/poll and remove()
     *   termination-> awaitTermination
     *
     * A null field is an NPE, which the section prints rather than swallows.
     */
    static void realFieldsAreInitialised() throws Exception {
        ThreadPoolExecutor tpe = (ThreadPoolExecutor) Executors.newFixedThreadPool(2);
        try {
            System.out.println("fields.ctl.isShutdown=" + tpe.isShutdown()
                    + " isTerminating=" + tpe.isTerminating()
                    + " active=" + tpe.getActiveCount());
            System.out.println("fields.mainLock.pool=" + tpe.getPoolSize()
                    + " tasks=" + tpe.getTaskCount()
                    + " largest=" + tpe.getLargestPoolSize());
            BlockingQueue<Runnable> q = tpe.getQueue();
            Runnable marker = new Runnable() {
                @Override public void run() { /* never executed: never handed to the pool */ }
            };
            System.out.println("fields.workQueue.offer=" + q.offer(marker)
                    + " size=" + q.size()
                    + " remove=" + tpe.remove(marker)
                    + " sizeAfter=" + q.size());
            System.out.println("fields.prestart=" + tpe.prestartCoreThread());
            System.out.println("fields.prestartAll=" + tpe.prestartAllCoreThreads());
            System.out.println("fields.workers.poolAfterPrestart=" + tpe.getPoolSize());
            // termination: a pool with nothing queued terminates once shut down.
            tpe.shutdown();
            System.out.println("fields.termination.await=" + tpe.awaitTermination(10, TimeUnit.SECONDS));
            System.out.println("fields.termination.terminated=" + tpe.isTerminated()
                    + " poolAfter=" + tpe.getPoolSize());
        } finally {
            tpe.shutdownNow();
        }
    }

    // ------------------------------------------------------------------
    // The three guards.
    // ------------------------------------------------------------------

    /**
     * An inline fallback passes any test that only checks the task ran. Assert
     * the executing thread is not the submitting thread — for `execute`, for
     * `submit(Runnable)` and for `submit(Callable)`, because they reach
     * dispatch by different routes.
     */
    static void asyncGuard() throws Exception {
        ExecutorService es = Executors.newFixedThreadPool(2);
        try {
            final Thread submitter = Thread.currentThread();
            final CountDownLatch ran = new CountDownLatch(1);
            final AtomicInteger offCallerThread = new AtomicInteger();
            es.execute(new Runnable() {
                @Override public void run() {
                    if (Thread.currentThread() != submitter) offCallerThread.incrementAndGet();
                    ran.countDown();
                }
            });
            System.out.println("async.execute.ran=" + ran.await(10, TimeUnit.SECONDS));
            System.out.println("async.execute.offCallerThread=" + (offCallerThread.get() == 1));

            Future<Boolean> f1 = es.submit(new Callable<Boolean>() {
                @Override public Boolean call() { return Thread.currentThread() != submitter; }
            });
            System.out.println("async.submitCallable.offCallerThread=" + f1.get(10, TimeUnit.SECONDS));

            final AtomicInteger off2 = new AtomicInteger();
            Future<?> f2 = es.submit(new Runnable() {
                @Override public void run() {
                    if (Thread.currentThread() != submitter) off2.incrementAndGet();
                }
            });
            f2.get(10, TimeUnit.SECONDS);
            System.out.println("async.submitRunnable.offCallerThread=" + (off2.get() == 1));
            System.out.println("async.submitRunnable.futureDone=" + f2.isDone()
                    + " cancelled=" + f2.isCancelled());

            // A latch that only the pool can release: if `execute` ran inline
            // this would deadlock rather than fail, so bound it.
            final CountDownLatch gate = new CountDownLatch(4);
            for (int i = 0; i < 4; i++) {
                es.execute(new Runnable() {
                    @Override public void run() { gate.countDown(); }
                });
            }
            System.out.println("async.latch=" + gate.await(10, TimeUnit.SECONDS));
        } finally {
            es.shutdown();
        }
    }

    /**
     * The failure this guards against is a native `execute()` implementation
     * re-entering itself: a real stack overflow and a process abort, not a
     * catchable `StackOverflowError`. Reached by executing from INSIDE a
     * worker — onto the same pool and onto a second one — which is the shape
     * that made the original `invoke_virtual` recursion fire.
     */
    static void selfRecursionGuard() throws Exception {
        ExecutorService outer = Executors.newFixedThreadPool(2);
        final ExecutorService inner = Executors.newCachedThreadPool();
        try {
            final CountDownLatch done = new CountDownLatch(2);
            final AtomicInteger depth = new AtomicInteger();
            outer.execute(new Runnable() {
                @Override public void run() {
                    depth.incrementAndGet();
                    inner.execute(new Runnable() {
                        @Override public void run() {
                            depth.incrementAndGet();
                            done.countDown();
                        }
                    });
                    done.countDown();
                }
            });
            System.out.println("recursion.crossPool=" + done.await(10, TimeUnit.SECONDS)
                    + " depth=" + (depth.get() == 2));

            final ExecutorService self = Executors.newFixedThreadPool(2);
            final CountDownLatch nested = new CountDownLatch(1);
            self.execute(new Runnable() {
                @Override public void run() {
                    self.execute(new Runnable() {
                        @Override public void run() { nested.countDown(); }
                    });
                }
            });
            System.out.println("recursion.samePool=" + nested.await(10, TimeUnit.SECONDS));
            self.shutdown();
        } finally {
            outer.shutdown();
            inner.shutdown();
        }
    }

    /**
     * One polymorphic `execute` call site, several receiver shapes rotated
     * through it: a factory pool, a directly-constructed pool, a user subclass
     * that overrides `execute`, and a hand-written `Executor` that is not a
     * pool at all. A dispatch cache keyed by (call site, receiver class) that
     * caches the first answer serves it to all four.
     */
    static void cachePoisoningGuard() throws Exception {
        final AtomicInteger subclassSaw = new AtomicInteger();
        ThreadPoolExecutor factory = (ThreadPoolExecutor) Executors.newFixedThreadPool(2);
        ThreadPoolExecutor direct = new ThreadPoolExecutor(
                2, 2, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
        ThreadPoolExecutor subclass = new ThreadPoolExecutor(
                2, 2, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>()) {
            @Override public void execute(Runnable command) {
                subclassSaw.incrementAndGet();
                super.execute(command);
            }
        };
        java.util.concurrent.Executor inlineExecutor = new java.util.concurrent.Executor() {
            @Override public void execute(Runnable command) { command.run(); }
        };
        try {
            List<java.util.concurrent.Executor> rota = new ArrayList<>();
            for (int round = 0; round < 3; round++) {
                rota.add(factory);
                rota.add(direct);
                rota.add(subclass);
                rota.add(inlineExecutor);
            }
            final CountDownLatch all = new CountDownLatch(rota.size());
            final AtomicInteger ran = new AtomicInteger();
            for (java.util.concurrent.Executor e : rota) {
                // ONE call site, four receiver classes.
                e.execute(new Runnable() {
                    @Override public void run() { ran.incrementAndGet(); all.countDown(); }
                });
            }
            System.out.println("poison.allRan=" + all.await(10, TimeUnit.SECONDS)
                    + " count=" + (ran.get() == rota.size()));
            System.out.println("poison.subclassOverrideSeen=" + (subclassSaw.get() == 3));
        } finally {
            factory.shutdown();
            direct.shutdown();
            subclass.shutdown();
        }
    }

    // ------------------------------------------------------------------
    // Lifecycle and configuration.
    // ------------------------------------------------------------------

    static void lifecycle() throws Exception {
        ThreadPoolExecutor tpe = (ThreadPoolExecutor) Executors.newFixedThreadPool(2);
        final CountDownLatch ran = new CountDownLatch(6);
        for (int i = 0; i < 6; i++) {
            tpe.execute(new Runnable() {
                @Override public void run() { ran.countDown(); }
            });
        }
        System.out.println("life.ranAll=" + ran.await(10, TimeUnit.SECONDS));
        tpe.shutdown();
        System.out.println("life.shutdownFlag=" + tpe.isShutdown());
        System.out.println("life.await=" + tpe.awaitTermination(10, TimeUnit.SECONDS));
        System.out.println("life.terminated=" + tpe.isTerminated()
                + " terminating=" + tpe.isTerminating()
                + " pool=" + tpe.getPoolSize());
        System.out.println("life.completed=" + tpe.getCompletedTaskCount()
                + " tasks=" + tpe.getTaskCount());
        System.out.println("life.shutdownNowAfterTerminate=" + tpe.shutdownNow().size());
        boolean rejected = false;
        try {
            tpe.execute(new Runnable() {
                @Override public void run() { }
            });
        } catch (java.util.concurrent.RejectedExecutionException e) {
            rejected = true;
        }
        System.out.println("life.rejectsAfterShutdown=" + rejected);

        // shutdownNow returns the queued-but-unstarted tasks. A pool with one
        // thread parked on a gate and two more tasks queued behind it hands
        // back exactly those two.
        ThreadPoolExecutor blocked = new ThreadPoolExecutor(
                1, 1, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
        final CountDownLatch hold = new CountDownLatch(1);
        final CountDownLatch entered = new CountDownLatch(1);
        blocked.execute(new Runnable() {
            @Override public void run() {
                entered.countDown();
                try { hold.await(10, TimeUnit.SECONDS); } catch (InterruptedException ignored) { }
            }
        });
        System.out.println("life.blocked.entered=" + entered.await(10, TimeUnit.SECONDS));
        blocked.execute(new Runnable() { @Override public void run() { } });
        blocked.execute(new Runnable() { @Override public void run() { } });
        List<Runnable> drained = blocked.shutdownNow();
        System.out.println("life.blocked.drained=" + drained.size());
        hold.countDown();
        System.out.println("life.blocked.await=" + blocked.awaitTermination(10, TimeUnit.SECONDS));
    }

    static void threadFactoryIsHonoured() throws Exception {
        final AtomicInteger created = new AtomicInteger();
        ThreadFactory tf = new ThreadFactory() {
            @Override public Thread newThread(Runnable r) {
                created.incrementAndGet();
                Thread t = new Thread(r, "l10-probe-worker");
                t.setDaemon(true);
                return t;
            }
        };
        ExecutorService es = Executors.newCachedThreadPool(tf);
        try {
            System.out.println("factory.installed=" + (((ThreadPoolExecutor) es).getThreadFactory() == tf));
            final CountDownLatch ran = new CountDownLatch(1);
            final AtomicInteger sawName = new AtomicInteger();
            final AtomicInteger sawDaemon = new AtomicInteger();
            es.execute(new Runnable() {
                @Override public void run() {
                    if ("l10-probe-worker".equals(Thread.currentThread().getName())) sawName.incrementAndGet();
                    if (Thread.currentThread().isDaemon()) sawDaemon.incrementAndGet();
                    ran.countDown();
                }
            });
            System.out.println("factory.ran=" + ran.await(10, TimeUnit.SECONDS));
            System.out.println("factory.usedOurThread=" + (sawName.get() == 1)
                    + " daemon=" + (sawDaemon.get() == 1));
            System.out.println("factory.createdAtLeastOne=" + (created.get() >= 1));
        } finally {
            es.shutdown();
        }
    }

    /**
     * `ScheduledThreadPoolExecutor` shares the `ThreadPoolExecutor` field
     * family, and its factory shortcut takes the same construction path. It is
     * here so a fix that repairs the plain pools and leaves the scheduled one
     * half-real does not read as green.
     */
    static void scheduledShape() throws Exception {
        ScheduledExecutorService ses = Executors.newScheduledThreadPool(2);
        try {
            System.out.println("sched.class=" + ses.getClass().getName());
            ThreadPoolExecutor tpe = (ThreadPoolExecutor) ses;
            System.out.println("sched.core=" + tpe.getCorePoolSize()
                    + " max=" + tpe.getMaximumPoolSize());
            System.out.println("sched.queue=" + tpe.getQueue().getClass().getName());
            System.out.println("sched.handler=" + tpe.getRejectedExecutionHandler().getClass().getName());
            final CountDownLatch ran = new CountDownLatch(1);
            ScheduledFuture<?> sf = ses.schedule(new Runnable() {
                @Override public void run() { ran.countDown(); }
            }, 1, TimeUnit.MILLISECONDS);
            System.out.println("sched.ran=" + ran.await(10, TimeUnit.SECONDS));
            sf.get(10, TimeUnit.SECONDS);
            System.out.println("sched.futureDone=" + sf.isDone());
            SynchronousQueue<Integer> sq = new SynchronousQueue<>();
            System.out.println("sched.sqRemaining=" + sq.remainingCapacity());
        } finally {
            ses.shutdown();
            System.out.println("sched.await=" + ses.awaitTermination(10, TimeUnit.SECONDS));
        }
    }
}
