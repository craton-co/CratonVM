import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;

/** L6 — the `java.util.concurrent.ForkJoinTask` and `ForkJoinPool` triples the
 *  `--jdk-only-report` marks `outcome=native-won`.
 *
 *  Method: ask the CONTRACT EDGES. For this pair the edges are the COMPLETION
 *  STATUS ALGEBRA (`isDone` / `isCancelled` / `isCompletedNormally` /
 *  `isCompletedAbnormally` / `getException`), which is one three-valued state
 *  reported through five predicates, and the pool's ARGUMENT VALIDATION and
 *  SHUTDOWN state machine.
 *
 *  DETERMINISM. Nothing prints a pool size, a thread name, a steal count, an
 *  active-thread count, a queued-task count, a timing, or any number the
 *  scheduler chooses. Parallelism is asked as a BOUND (`>= 1`) and never as a
 *  value, because it is `availableProcessors`-derived. Every task is a pure
 *  function of its input and every result is read after a join. The common pool
 *  is never shut down until the very last row, and that row exists precisely
 *  because the JDK specifies the shutdown as a NO-OP.
 */
public class ForkJoinShadowSweep {
    static int rows = 0;

    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    /** Also prints the CAUSE type: the whole point of `ExecutionException` and
     *  of `invoke`'s rethrow is which exception survives the boundary. */
    static void tc(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName()
                 + (e.getCause() == null ? "" : " cause " + e.getCause().getClass().getName()));
        }
    }
    interface ThrowingRun { void run() throws Throwable; }

    /** A task that is a pure function of its argument. */
    static final class Sum extends RecursiveTask<Integer> {
        private final int n;
        Sum(int n) { this.n = n; }
        protected Integer compute() {
            if (n <= 1) return n;
            Sum a = new Sum(n - 1);
            a.fork();
            return n + a.join();
        }
    }
    static final class Boom extends RecursiveTask<Integer> {
        protected Integer compute() { throw new IllegalStateException("l6-boom"); }
    }
    static final class BoomAction extends RecursiveAction {
        protected void compute() { throw new ArithmeticException("l6-boom-action"); }
    }
    static final class Nop extends RecursiveAction {
        protected void compute() { }
    }
    /** Blocks until released; used only where a NOT-YET-DONE task is required,
     *  and always released and joined before anything is read. */
    static final class Blocker extends RecursiveAction {
        final CountDownLatch go = new CountDownLatch(1);
        protected void compute() {
            try { go.await(5, TimeUnit.SECONDS); } catch (InterruptedException ignored) { }
        }
    }

    // ---- the completion status algebra ----------------------------------
    static void statusAlgebra() throws Exception {
        // (1) NOT YET RUN. All five predicates have a defined answer.
        Nop fresh = new Nop();
        p("fresh isDone", fresh.isDone());
        p("fresh isCancelled", fresh.isCancelled());
        p("fresh isCompletedNormally", fresh.isCompletedNormally());
        p("fresh isCompletedAbnormally", fresh.isCompletedAbnormally());
        p("fresh getException", fresh.getException());
        p("fresh getRawResult", fresh.getRawResult());

        // (2) COMPLETED NORMALLY.
        Sum ok = new Sum(6);
        p("invoke result", ok.invoke());
        p("normal isDone", ok.isDone());
        p("normal isCancelled", ok.isCancelled());
        p("normal isCompletedNormally", ok.isCompletedNormally());
        p("normal isCompletedAbnormally", ok.isCompletedAbnormally());
        p("normal getException", ok.getException());
        p("normal getRawResult", ok.getRawResult());
        p("normal join", ok.join());
        p("normal get", ok.get());

        // (3) COMPLETED ABNORMALLY — the exception must survive as ITSELF from
        // invoke/join, and WRAPPED from get(). Those are two different
        // contracts on one state and a shim usually implements only one.
        Boom bad = new Boom();
        tc("invoke of a throwing task", () -> bad.invoke());
        p("abnormal isDone", bad.isDone());
        p("abnormal isCancelled", bad.isCancelled());
        p("abnormal isCompletedNormally", bad.isCompletedNormally());
        p("abnormal isCompletedAbnormally", bad.isCompletedAbnormally());
        p("abnormal getException type",
          bad.getException() == null ? "null" : bad.getException().getClass().getName());
        p("abnormal getException message",
          bad.getException() == null ? "null" : bad.getException().getMessage());
        tc("abnormal join", () -> bad.join());
        tc("abnormal get", () -> bad.get());
        tc("abnormal get timed", () -> bad.get(1, TimeUnit.SECONDS));
        t("abnormal quietlyJoin does not throw", () -> bad.quietlyJoin());

        // (4) CANCELLED BEFORE RUNNING. isCancelled implies isCompletedAbnormally
        // but NOT the reverse — the distinction W3-4 was written about.
        Nop c = new Nop();
        p("cancel(false) on a fresh task", c.cancel(false));
        p("cancelled isDone", c.isDone());
        p("cancelled isCancelled", c.isCancelled());
        p("cancelled isCompletedNormally", c.isCompletedNormally());
        p("cancelled isCompletedAbnormally", c.isCompletedAbnormally());
        p("cancelled getException type",
          c.getException() == null ? "null" : c.getException().getClass().getName());
        tc("cancelled join", () -> c.join());
        tc("cancelled get", () -> c.get());
        t("cancelled quietlyJoin", () -> c.quietlyJoin());
        p("cancel again on a cancelled task", c.cancel(true));
        p("invoke of an already-cancelled task", invokeCatch(new Nop()));

        // (5) CANCEL AFTER COMPLETION must return false and change nothing.
        Sum done = new Sum(3);
        done.invoke();
        p("cancel after normal completion", done.cancel(true));
        p("still isCompletedNormally", done.isCompletedNormally());
        p("still not isCancelled", done.isCancelled());
        Boom done2 = new Boom();
        try { done2.invoke(); } catch (Throwable ignored) { }
        p("cancel after abnormal completion", done2.cancel(true));
        p("still isCompletedAbnormally", done2.isCompletedAbnormally());
        p("still not isCancelled after abnormal", done2.isCancelled());
    }
    static String invokeCatch(ForkJoinTask<?> task) {
        task.cancel(false);
        try { task.invoke(); return "no-throw"; }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }

    // ---- complete / completeExceptionally / reinitialize ------------------
    static void completion() throws Exception {
        RecursiveTask<Integer> t1 = new RecursiveTask<>() {
            protected Integer compute() { return 1; }
        };
        t1.complete(42);
        p("complete(v) makes it done", t1.isDone());
        p("complete(v) join", t1.join());
        p("complete(v) isCompletedNormally", t1.isCompletedNormally());
        p("complete(v) getRawResult", t1.getRawResult());
        t1.complete(43);
        p("second complete is ignored", t1.join());

        RecursiveTask<Integer> t2 = new RecursiveTask<>() {
            protected Integer compute() { return 1; }
        };
        t2.completeExceptionally(new IllegalArgumentException("l6-ce"));
        p("completeExceptionally isDone", t2.isDone());
        p("completeExceptionally isCompletedAbnormally", t2.isCompletedAbnormally());
        p("completeExceptionally isCancelled", t2.isCancelled());
        p("completeExceptionally getException",
          t2.getException() == null ? "null" : t2.getException().getClass().getName());
        tc("completeExceptionally join", () -> t2.join());
        t("completeExceptionally(null)", () -> {
            RecursiveTask<Integer> x = new RecursiveTask<>() {
                protected Integer compute() { return 1; }
            };
            x.completeExceptionally(null);
        });
        // completeExceptionally with a CancellationException must report as
        // cancelled — the status is derived from the exception TYPE.
        RecursiveTask<Integer> t3 = new RecursiveTask<>() {
            protected Integer compute() { return 1; }
        };
        t3.completeExceptionally(new CancellationException("l6-cx"));
        p("completeExceptionally(CE) isCancelled", t3.isCancelled());
        p("completeExceptionally(CE) isCompletedAbnormally", t3.isCompletedAbnormally());

        // reinitialize clears the status so the task can run again.
        Sum r = new Sum(4);
        p("reinit first invoke", r.invoke());
        r.reinitialize();
        p("after reinitialize isDone", r.isDone());
        p("after reinitialize getRawResult", r.getRawResult());
        p("reinit second invoke", r.invoke());
        Nop rc = new Nop();
        rc.cancel(false);
        rc.reinitialize();
        p("reinitialize clears cancellation", rc.isCancelled());
        p("reinitialize clears done", rc.isDone());
    }

    // ---- the task tag ------------------------------------------------------
    static void tags() {
        Nop x = new Nop();
        p("default tag", x.getForkJoinTaskTag());
        p("setForkJoinTaskTag returns previous", x.setForkJoinTaskTag((short) 7));
        p("tag after set", x.getForkJoinTaskTag());
        p("CAS tag wrong expect", x.compareAndSetForkJoinTaskTag((short) 3, (short) 9));
        p("tag after failed CAS", x.getForkJoinTaskTag());
        p("CAS tag right expect", x.compareAndSetForkJoinTaskTag((short) 7, (short) 9));
        p("tag after successful CAS", x.getForkJoinTaskTag());
        p("negative tag round-trips", roundTripTag((short) -1));
        p("Short.MIN_VALUE tag round-trips", roundTripTag(Short.MIN_VALUE));
        p("Short.MAX_VALUE tag round-trips", roundTripTag(Short.MAX_VALUE));
    }
    static short roundTripTag(short v) {
        Nop x = new Nop();
        x.setForkJoinTaskTag(v);
        return x.getForkJoinTaskTag();
    }

    // ---- adapters ----------------------------------------------------------
    static void adapters() throws Exception {
        final AtomicInteger ran = new AtomicInteger();
        ForkJoinTask<?> ra = ForkJoinTask.adapt(() -> ran.incrementAndGet());
        p("adapt(Runnable) getRawResult before", ra.getRawResult());
        ra.invoke();
        p("adapt(Runnable) ran", ran.get());
        p("adapt(Runnable) getRawResult", ra.getRawResult());
        p("adapt(Runnable) isCompletedNormally", ra.isCompletedNormally());

        ForkJoinTask<String> rv = ForkJoinTask.adapt(() -> { }, "fixed");
        p("adapt(Runnable, result) invoke", rv.invoke());

        ForkJoinTask<Integer> ca = ForkJoinTask.adapt((Callable<Integer>) () -> 11);
        p("adapt(Callable) invoke", ca.invoke());
        ForkJoinTask<Integer> cb = ForkJoinTask.adapt((Callable<Integer>) () -> {
            throw new java.io.IOException("l6-checked");
        });
        // A CHECKED exception from a Callable comes back from join() as
        // RuntimeException-wrapped in the JDK; get() wraps it in
        // ExecutionException. Which wrapper appears where is the contract.
        //
        // `invoke()` FIRST, deliberately: `join()` on a task that was never
        // forked or invoked blocks until the task completes, and a task nobody
        // scheduled never does. That is not a defect to report -- it is the
        // specified behaviour -- but it hangs a probe, which is why every
        // `join`/`get` row in this file names a task that has already been run,
        // cancelled or completed by hand.
        tc("adapt(Callable) that throws checked - invoke", () -> cb.invoke());
        tc("adapt(Callable) that throws checked - join", () -> cb.join());
        tc("adapt(Callable) that throws checked - get", () -> cb.get());
        t("adapt((Runnable) null)", () -> ForkJoinTask.adapt((Runnable) null));
        t("adapt((Callable) null)", () -> ForkJoinTask.adapt((Callable<Integer>) null));
        ForkJoinTask<Integer> ic = ForkJoinTask.adaptInterruptible((Callable<Integer>) () -> 5);
        p("adaptInterruptible invoke", ic.invoke());
    }

    // ---- outside a pool ----------------------------------------------------
    static void outsideAPool() throws Exception {
        p("inForkJoinPool from main", ForkJoinTask.inForkJoinPool());
        p("getPool from main", ForkJoinTask.getPool());
        p("getQueuedTaskCount from main", ForkJoinTask.getQueuedTaskCount());
        p("getSurplusQueuedTaskCount from main", ForkJoinTask.getSurplusQueuedTaskCount());
        // fork() from OUTSIDE a pool is specified to submit to the common pool
        // rather than to throw; the assertion is that the result arrives.
        Sum s = new Sum(5);
        s.fork();
        p("fork from main then join", s.join());
        p("forked task isCompletedNormally", s.isCompletedNormally());
        // tryUnfork on a task that was never forked must answer false.
        p("tryUnfork a never-forked task", new Nop().tryUnfork());
        // helpQuiesce from outside a pool is legal.
        t("helpQuiesce from main", () -> ForkJoinTask.helpQuiesce());

        // invokeAll: the static two-arg and the varargs form. Both must run
        // every task, and both must rethrow the FIRST exception.
        Nop a = new Nop(), b = new Nop();
        t("invokeAll(t1, t2)", () -> ForkJoinTask.invokeAll(a, b));
        p("invokeAll both done", a.isDone() && b.isDone());
        // `t`, not `tc`. Whether the JDK hands back the task's OWN throwable
        // or a same-class COPY with it as the cause depends on which thread ran
        // the failing task, and HotSpot answers BOTH: 3 of 8 runs of
        // `probes/FjCauseVar`-shaped code copied, 5 did not, on one idle host.
        // The TYPE is the contract; the cause is a scheduling artefact and
        // printing it would be a probe that reports the host's load. Its
        // sibling `pool.invoke` row below IS asked with `tc`, because that one
        // copied 8/8.
        t("invokeAll with a thrower", () -> ForkJoinTask.invokeAll(new Nop(), new BoomAction()));
        Nop c1 = new Nop(), c2 = new Nop(), c3 = new Nop();
        t("invokeAll varargs", () -> ForkJoinTask.invokeAll(c1, c2, c3));
        p("invokeAll varargs all done", c1.isDone() && c2.isDone() && c3.isDone());
        List<Nop> lst = new ArrayList<>(List.of(new Nop(), new Nop()));
        t("invokeAll(Collection)", () -> ForkJoinTask.invokeAll(lst));
        p("invokeAll(Collection) all done", lst.get(0).isDone() && lst.get(1).isDone());
        t("invokeAll(null, null)", () -> ForkJoinTask.invokeAll((ForkJoinTask<?>) null, null));
    }

    // ---- inside a pool -----------------------------------------------------
    static void insideAPool() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        try {
            Boolean in = pool.invoke(new RecursiveTask<Boolean>() {
                protected Boolean compute() { return ForkJoinTask.inForkJoinPool(); }
            });
            p("inForkJoinPool inside", in);
            Boolean sameP = pool.invoke(new RecursiveTask<Boolean>() {
                protected Boolean compute() { return ForkJoinTask.getPool() != null; }
            });
            p("getPool inside is non-null", sameP);
            Boolean owner = pool.invoke(new RecursiveTask<Boolean>() {
                protected Boolean compute() { return getPool() == ForkJoinTask.getPool(); }
            });
            p("task getPool matches static getPool", owner);
            p("nested fork/join result", pool.invoke(new Sum(10)));
            Integer q = pool.invoke(new RecursiveTask<Integer>() {
                protected Integer compute() { return getQueuedTaskCount(); }
            });
            p("getQueuedTaskCount inside is non-negative", q >= 0);
        } finally {
            pool.shutdown();
            pool.awaitTermination(5, TimeUnit.SECONDS);
        }
    }

    // ---- ForkJoinPool: construction ---------------------------------------
    static void poolConstruction() throws Exception {
        t("new ForkJoinPool(0)", () -> new ForkJoinPool(0).shutdown());
        t("new ForkJoinPool(-1)", () -> new ForkJoinPool(-1).shutdown());
        t("new ForkJoinPool(Integer.MAX_VALUE)",
          () -> new ForkJoinPool(Integer.MAX_VALUE).shutdown());
        t("new ForkJoinPool(1, null, null, false)",
          () -> new ForkJoinPool(1, null, null, false).shutdown());
        ForkJoinPool p1 = new ForkJoinPool(1);
        try {
            p("parallelism 1", p1.getParallelism());
            p("getAsyncMode false by default", p1.getAsyncMode());
            p("isShutdown fresh", p1.isShutdown());
            p("isTerminated fresh", p1.isTerminated());
            p("isTerminating fresh", p1.isTerminating());
            p("isQuiescent fresh", p1.isQuiescent());
            p("hasQueuedSubmissions fresh", p1.hasQueuedSubmissions());
            p("getFactory non-null", p1.getFactory() != null);
            p("getUncaughtExceptionHandler default", p1.getUncaughtExceptionHandler());
            p("getPoolSize non-negative", p1.getPoolSize() >= 0);
            p("getRunningThreadCount non-negative", p1.getRunningThreadCount() >= 0);
            p("getActiveThreadCount non-negative", p1.getActiveThreadCount() >= 0);
            p("getStealCount non-negative", p1.getStealCount() >= 0);
            p("getQueuedTaskCount non-negative", p1.getQueuedTaskCount() >= 0);
            p("getQueuedSubmissionCount non-negative", p1.getQueuedSubmissionCount() >= 0);
        } finally {
            p1.shutdown();
            p1.awaitTermination(5, TimeUnit.SECONDS);
        }
        ForkJoinPool async = new ForkJoinPool(1, ForkJoinPool.defaultForkJoinWorkerThreadFactory,
                                              null, true);
        try { p("getAsyncMode true", async.getAsyncMode()); }
        finally { async.shutdown(); async.awaitTermination(5, TimeUnit.SECONDS); }
    }

    // ---- ForkJoinPool: submission argument contract -------------------------
    static void poolSubmission() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        try {
            p("submit(Callable) get", pool.submit(() -> 3).get());
            p("submit(Runnable, result) get", pool.submit(() -> { }, "r").get());
            p("submit(ForkJoinTask) join", pool.submit(new Sum(5)).join());
            p("invoke(task)", pool.invoke(new Sum(4)));
            t("execute(Runnable)", () -> pool.execute(() -> { }));
            t("execute(ForkJoinTask)", () -> pool.execute(new Nop()));
            t("submit((Callable) null)", () -> pool.submit((Callable<Integer>) null));
            t("submit((Runnable) null)", () -> pool.submit((Runnable) null));
            t("submit((ForkJoinTask) null)", () -> pool.submit((ForkJoinTask<Integer>) null));
            t("execute((Runnable) null)", () -> pool.execute((Runnable) null));
            t("execute((ForkJoinTask) null)", () -> pool.execute((ForkJoinTask<?>) null));
            t("invoke(null)", () -> pool.invoke(null));
            // `tc`, unlike `invokeAll with a thrower` above. MEASURED over
            // eight HotSpot runs: `pool.invoke` copies the exception 8/8 and
            // `invokeAll` copies it 3/8, so this one IS a stable contract and
            // that one is not. The pool must hand back a same-class copy whose
            // cause is the original.
            tc("invoke of a throwing task", () -> pool.invoke(new Boom()));
            tc("submit of a throwing task then get", () -> pool.submit(new Boom()).get());
            List<Callable<Integer>> cs = new ArrayList<>();
            cs.add(() -> 1); cs.add(() -> 2);
            List<Future<Integer>> fs = pool.invokeAll(cs);
            List<String> vals = new ArrayList<>();
            for (Future<Integer> f : fs) vals.add(String.valueOf(f.get()));
            p("invokeAll(Collection<Callable>)", vals);
            t("invokeAll(null)", () -> pool.invokeAll(null));
            p("invokeAny", pool.invokeAny(cs) >= 1);
            p("getParallelism >= 1", pool.getParallelism() >= 1);
        } finally {
            pool.shutdown();
            pool.awaitTermination(5, TimeUnit.SECONDS);
        }
    }

    // ---- ForkJoinPool: the shutdown state machine ---------------------------
    static void poolShutdown() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        pool.invoke(new Sum(3));
        p("before shutdown isShutdown", pool.isShutdown());
        pool.shutdown();
        p("after shutdown isShutdown", pool.isShutdown());
        p("awaitTermination(5s) after shutdown", pool.awaitTermination(5, TimeUnit.SECONDS));
        p("after termination isTerminated", pool.isTerminated());
        p("after termination isTerminating", pool.isTerminating());
        // Submitting to a terminated pool is RejectedExecutionException.
        t("submit after shutdown", () -> pool.submit(() -> 1));
        t("execute after shutdown", () -> pool.execute(() -> { }));
        t("invoke after shutdown", () -> pool.invoke(new Nop()));
        t("second shutdown", () -> pool.shutdown());
        p("shutdownNow on a terminated pool", pool.shutdownNow());
        p("awaitTermination(0) on a terminated pool",
          pool.awaitTermination(0, TimeUnit.SECONDS));
        p("awaitTermination(-1) on a terminated pool",
          pool.awaitTermination(-1, TimeUnit.SECONDS));
        t("awaitTermination(1, null)", () -> pool.awaitTermination(1, null));
        p("awaitQuiescence on a terminated pool",
          pool.awaitQuiescence(1, TimeUnit.SECONDS));

        // shutdownNow on a pool with a BLOCKED task: the task is released first
        // so the probe cannot hang, and only the returned-list SHAPE is read.
        ForkJoinPool p2 = new ForkJoinPool(1);
        Blocker b = new Blocker();
        p2.execute(b);
        b.go.countDown();
        List<Runnable> left = p2.shutdownNow();
        p("shutdownNow returns a list", left != null);
        p("after shutdownNow isShutdown", p2.isShutdown());
        p("shutdownNow pool terminates", p2.awaitTermination(5, TimeUnit.SECONDS));
    }

    // ---- the common pool ----------------------------------------------------
    static void commonPool() throws Exception {
        p("commonPool non-null", ForkJoinPool.commonPool() != null);
        p("commonPool is a singleton",
          ForkJoinPool.commonPool() == ForkJoinPool.commonPool());
        p("commonPoolParallelism >= 0", ForkJoinPool.getCommonPoolParallelism() >= 0);
        p("commonPool parallelism matches static",
          ForkJoinPool.commonPool().getParallelism()
              == ForkJoinPool.getCommonPoolParallelism()
          || ForkJoinPool.getCommonPoolParallelism() == 0);
        p("commonPool runs a task", ForkJoinPool.commonPool().invoke(new Sum(7)));
        p("commonPool isShutdown before", ForkJoinPool.commonPool().isShutdown());
        // The JDK specifies BOTH of these as no-ops on the common pool. A shim
        // that implements them literally takes out every later user of it — so
        // this is deliberately the last thing the probe does before its own
        // re-check.
        t("commonPool shutdown is a no-op", () -> ForkJoinPool.commonPool().shutdown());
        p("commonPool isShutdown after shutdown()",
          ForkJoinPool.commonPool().isShutdown());
        p("commonPool shutdownNow returns empty",
          ForkJoinPool.commonPool().shutdownNow().isEmpty());
        p("commonPool isShutdown after shutdownNow",
          ForkJoinPool.commonPool().isShutdown());
        p("commonPool isTerminated", ForkJoinPool.commonPool().isTerminated());
        p("commonPool still runs a task after shutdown",
          ForkJoinPool.commonPool().invoke(new Sum(4)));
    }

    // ---- ManagedBlocker -----------------------------------------------------
    static void managedBlocker() throws Exception {
        final AtomicInteger releases = new AtomicInteger();
        ForkJoinPool.ManagedBlocker mb = new ForkJoinPool.ManagedBlocker() {
            private boolean done = false;
            public boolean block() { done = true; releases.incrementAndGet(); return true; }
            public boolean isReleasable() { return done; }
        };
        t("managedBlock from main", () -> ForkJoinPool.managedBlock(mb));
        p("managedBlock ran block()", releases.get());
        t("managedBlock(null)", () -> ForkJoinPool.managedBlock(null));
        // An ALREADY-releasable blocker must not call block() at all.
        final AtomicInteger n2 = new AtomicInteger();
        ForkJoinPool.ManagedBlocker ready = new ForkJoinPool.ManagedBlocker() {
            public boolean block() { n2.incrementAndGet(); return true; }
            public boolean isReleasable() { return true; }
        };
        t("managedBlock of a releasable blocker", () -> ForkJoinPool.managedBlock(ready));
        p("releasable blocker never blocked", n2.get());
    }

    public static void main(String[] args) throws Exception {
        statusAlgebra();
        completion();
        tags();
        adapters();
        outsideAPool();
        insideAPool();
        poolConstruction();
        poolSubmission();
        poolShutdown();
        managedBlocker();
        commonPool();
        System.out.println("rows " + rows + " DONE ForkJoinShadowSweep");
    }
}
