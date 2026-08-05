import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.Future;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * API-fidelity matrix for {@code java.util.concurrent.ForkJoinPool} as reached
 * through {@code ForkJoinPool.commonPool()}.
 *
 * <p>Written for the CDI/Weld `invokeAll` RejectedExecutionException
 * investigation (docs/known-issues/hibernate/
 * cdi-cluster-forkjoinpool-invokeall-rejectedexecution-20260804.md). CratonVM's
 * `commonPool()` is a VM Bridge shortcut that never runs the real JDK pool
 * constructor, so any pool method NOT on the bridge allow-list falls through to
 * real JDK bytecode reading an under-initialized instance. That surface is only
 * discoverable by enumerating it, so this probe calls every public
 * ForkJoinPool entry point Weld or an app could plausibly reach and prints one
 * ROW per operation.
 *
 * <p>Run it on the HOST JDK first: the host output IS the contract. Every ROW
 * line is `ROW &lt;name&gt; = &lt;value&gt;` or `ROW &lt;name&gt; EXC
 * &lt;ExceptionClass&gt;`, so the two runs diff line-for-line.
 *
 * <p>Each row asserts PAIRED properties (a value AND the observable side
 * effect, e.g. "invokeAll returned 3 futures" AND "all 3 callables actually
 * ran" AND "every future reports done") rather than just "no exception" — a
 * bridge that silently drops work would otherwise score a false PASS.
 */
public class FjpMatrixProbe {

    /** Bumped by every Callable/Runnable the probe hands to the pool. */
    static final AtomicInteger RAN = new AtomicInteger();

    public static void main(String[] args) throws Exception {
        ForkJoinPool pool = ForkJoinPool.commonPool();

        // --- identity / read-only accessors -----------------------------
        row("commonPool.nonNull", pool != null);
        row("commonPool.identity", ForkJoinPool.commonPool() == ForkJoinPool.commonPool());
        row("getParallelism.positive", val(() -> pool.getParallelism() > 0));
        row("getCommonPoolParallelism.positive", val(() -> ForkJoinPool.getCommonPoolParallelism() > 0));
        row("getFactory.nonNull", val(() -> pool.getFactory() != null));
        row("getUncaughtExceptionHandler.ok", val(() -> {
            pool.getUncaughtExceptionHandler();
            return true;
        }));
        row("getPoolSize.nonNegative", val(() -> pool.getPoolSize() >= 0));
        row("getAsyncMode.ok", val(() -> {
            pool.getAsyncMode();
            return true;
        }));
        row("getRunningThreadCount.nonNegative", val(() -> pool.getRunningThreadCount() >= 0));
        row("getActiveThreadCount.nonNegative", val(() -> pool.getActiveThreadCount() >= 0));
        row("isQuiescent.ok", val(() -> {
            pool.isQuiescent();
            return true;
        }));
        row("getStealCount.nonNegative", val(() -> pool.getStealCount() >= 0));
        row("getQueuedTaskCount.nonNegative", val(() -> pool.getQueuedTaskCount() >= 0));
        row("getQueuedSubmissionCount.nonNegative", val(() -> pool.getQueuedSubmissionCount() >= 0));
        row("getDelayedTaskCount.nonNegative", val(() -> pool.getDelayedTaskCount() >= 0));
        row("hasQueuedSubmissions.ok", val(() -> {
            pool.hasQueuedSubmissions();
            return true;
        }));
        row("toString.nonEmpty", val(() -> pool.toString() != null && !pool.toString().isEmpty()));
        row("isShutdown.false", val(() -> !pool.isShutdown()));
        row("isTerminated.false", val(() -> !pool.isTerminated()));
        row("isTerminating.false", val(() -> !pool.isTerminating()));

        // --- single-task submission surfaces (already allow-listed) ------
        row("invoke.ForkJoinTask", val(() -> {
            int before = RAN.get();
            Integer r = pool.invoke(task(7));
            return r == 7 && RAN.get() == before + 1;
        }));
        row("submit.ForkJoinTask", val(() -> {
            int before = RAN.get();
            ForkJoinTask<Integer> t = pool.submit(task(8));
            return t.get(30, TimeUnit.SECONDS) == 8 && t.isDone() && RAN.get() == before + 1;
        }));
        row("submit.Callable", val(() -> {
            int before = RAN.get();
            ForkJoinTask<Integer> t = pool.submit(callable(9));
            return t.get(30, TimeUnit.SECONDS) == 9 && t.isDone() && RAN.get() == before + 1;
        }));
        row("submit.Runnable", val(() -> {
            int before = RAN.get();
            ForkJoinTask<?> t = pool.submit(runnable());
            t.get(30, TimeUnit.SECONDS);
            return t.isDone() && RAN.get() == before + 1;
        }));
        row("submit.RunnableT", val(() -> {
            int before = RAN.get();
            ForkJoinTask<String> t = pool.submit(runnable(), "fixed");
            return "fixed".equals(t.get(30, TimeUnit.SECONDS)) && RAN.get() == before + 1;
        }));
        row("externalSubmit.ForkJoinTask", val(() -> {
            int before = RAN.get();
            ForkJoinTask<Integer> t = pool.externalSubmit(task(10));
            return t.get(30, TimeUnit.SECONDS) == 10 && RAN.get() == before + 1;
        }));
        row("lazySubmit.ForkJoinTask", val(() -> {
            int before = RAN.get();
            ForkJoinTask<Integer> t = pool.lazySubmit(task(11));
            return t.get(30, TimeUnit.SECONDS) == 11 && RAN.get() == before + 1;
        }));
        row("execute.Runnable", val(() -> {
            int before = RAN.get();
            pool.execute(runnable());
            pool.awaitQuiescence(30, TimeUnit.SECONDS);
            return RAN.get() == before + 1;
        }));
        rowText("execute.ForkJoinTask", text(() -> {
            int before = RAN.get();
            ForkJoinTask<Integer> t = task(12);
            pool.execute(t);
            Integer joined = t.join();
            return "join=" + joined + " ran=" + (RAN.get() - before);
        }));
        // NOT `... || true` — that row can never fail. awaitQuiescence must
        // actually WAIT: execute(Runnable) runs on a real daemon thread, so a
        // pool that answers "quiescent" immediately loses the task's effect.
        rowText("awaitQuiescence.waitsForExecute", text(() -> {
            int before = RAN.get();
            pool.execute(slowRunnable());
            boolean quiesced = pool.awaitQuiescence(30, TimeUnit.SECONDS);
            return "quiesced=" + quiesced + " ran=" + (RAN.get() - before);
        }));

        // --- task-family exception propagation: a compute() that throws
        // --- must not be swallowed into a null result -------------------
        rowText("invoke.ForkJoinTask.onThrow", text(() -> {
            try {
                Integer v = pool.invoke(throwingTask());
                return "returned:" + v;
            } catch (Throwable t) {
                return "threw:" + t.getClass().getName();
            }
        }));
        rowText("submitForkJoinTask.join.onThrow", text(() -> {
            ForkJoinTask<Integer> t = throwingTask();
            try {
                Integer v = pool.submit(t).join();
                return "returned:" + v;
            } catch (Throwable e) {
                return "threw:" + e.getClass().getName();
            }
        }));
        rowText("forkJoinTask.getException.afterThrow", text(() -> {
            ForkJoinTask<Integer> t = throwingTask();
            try {
                pool.submit(t).join();
            } catch (Throwable ignored) {
                // expected
            }
            Throwable ex = t.getException();
            return "class=" + (ex == null ? "null" : ex.getClass().getName());
        }));
        rowText("forkJoinTask.getException.onSuccess", text(() -> {
            ForkJoinTask<Integer> t = task(31);
            pool.submit(t).join();
            return "class=" + (t.getException() == null ? "null" : "nonnull");
        }));
        rowText("forkJoinTask.isCompletedNormally.afterThrow", text(() -> {
            ForkJoinTask<Integer> t = throwingTask();
            try {
                pool.submit(t).join();
            } catch (Throwable ignored) {
                // expected
            }
            return "done=" + t.isDone() + " normally=" + t.isCompletedNormally();
        }));

        // --- the bulk surfaces: this is what Weld's ConcurrentBeanDeployer
        // --- actually calls (AbstractExecutorServices.invokeAll...) -----
        row("invokeAll.Collection", val(() -> {
            int before = RAN.get();
            List<Future<Integer>> fs = pool.invokeAll(callables(1, 2, 3));
            return fs.size() == 3
                && RAN.get() == before + 3
                && allDone(fs)
                && sum(fs) == 6;
        }));
        row("invokeAll.Collection.timed", val(() -> {
            int before = RAN.get();
            List<Future<Integer>> fs = pool.invokeAll(callables(4, 5), 30, TimeUnit.SECONDS);
            return fs.size() == 2
                && RAN.get() == before + 2
                && allDone(fs)
                && sum(fs) == 9;
        }));
        row("invokeAllUninterruptibly.Collection", val(() -> {
            int before = RAN.get();
            List<Future<Integer>> fs = pool.invokeAllUninterruptibly(callables(6, 7));
            return fs.size() == 2
                && RAN.get() == before + 2
                && allDone(fs)
                && sum(fs) == 13;
        }));
        row("invokeAll.empty", val(() -> pool.invokeAll(callables()).isEmpty()));
        // A null element is specified to raise NPE. Silently skipping it would
        // return a SHORTER list than the caller submitted — a silent wrong
        // answer of exactly the kind this whole investigation is about.
        rowText("invokeAll.nullElement", text(() -> {
            List<Callable<Integer>> withNull = new ArrayList<>();
            withNull.add(callable(1));
            withNull.add(null);
            List<Future<Integer>> fs = pool.invokeAll(withNull);
            return "returnedSize=" + fs.size();
        }));
        row("invokeAll.singleton", val(() -> {
            List<Future<Integer>> fs = pool.invokeAll(Collections.singletonList(callable(99)));
            return fs.size() == 1 && fs.get(0).get(30, TimeUnit.SECONDS) == 99;
        }));
        // invokeAny must return the value of SOME completed callable — a null
        // is a silent wrong answer, which reads identically to a pass in a
        // plain boolean row, so the predicate is spelled out.
        //
        // WHICH callable wins is deliberately NOT part of the row: the JDK
        // leaves it unspecified and HotSpot really does return a different
        // one run to run, so printing the raw value would make this row diff
        // against itself.
        rowText("invokeAny.Collection", text(() -> {
            int before = RAN.get();
            Integer r = pool.invokeAny(callables(21, 22, 23));
            return "inRange=" + (r != null && r >= 21 && r <= 23)
                + " ranAtLeastOne=" + (RAN.get() > before);
        }));
        rowText("invokeAny.Collection.timed", text(() -> {
            int before = RAN.get();
            Integer r = pool.invokeAny(callables(31, 32), 30, TimeUnit.SECONDS);
            return "inRange=" + (r != null && (r == 31 || r == 32))
                + " ranAtLeastOne=" + (RAN.get() > before);
        }));
        // invokeAny with an all-failing collection must throw
        // ExecutionException, not return null.
        rowText("invokeAny.empty", text(() -> {
            Integer r = pool.invokeAny(callables());
            return "returned:" + r;
        }));
        rowText("invokeAny.allFail", text(() -> {
            Integer r = pool.invokeAny(Arrays.<Callable<Integer>>asList(boom(), boom()));
            return "returned:" + r;
        }));

        // --- the interface-dispatch shape Weld actually uses:
        // --- an ExecutorService-typed receiver, invokeinterface ----------
        row("ExecutorService.invokeAll", val(() -> {
            ExecutorService es = ForkJoinPool.commonPool();
            int before = RAN.get();
            List<Future<Integer>> fs = es.invokeAll(callables(41, 42));
            return fs.size() == 2 && RAN.get() == before + 2 && allDone(fs) && sum(fs) == 83;
        }));
        row("ExecutorService.submit", val(() -> {
            ExecutorService es = ForkJoinPool.commonPool();
            return es.submit(callable(43)).get(30, TimeUnit.SECONDS) == 43;
        }));
        row("ExecutorService.execute", val(() -> {
            ExecutorService es = ForkJoinPool.commonPool();
            int before = RAN.get();
            es.execute(runnable());
            ForkJoinPool.commonPool().awaitQuiescence(30, TimeUnit.SECONDS);
            return RAN.get() == before + 1;
        }));

        // --- the exception-propagation contract Weld depends on:
        // --- invokeAllAndCheckForExceptions calls future.get() and expects
        // --- an ExecutionException wrapping the callable's throwable ------
        // Reports the OBSERVED shape, not a boolean: the host JDK is the
        // contract here and it is not the obvious one.
        rowText("invokeAll.futureGet.onFailure", text(() -> {
            List<Future<Integer>> fs = pool.invokeAll(
                Arrays.<Callable<Integer>>asList(callable(1), boom()));
            if (fs.size() != 2) {
                return "size=" + fs.size();
            }
            fs.get(0).get(30, TimeUnit.SECONDS);
            try {
                Integer v = fs.get(1).get(30, TimeUnit.SECONDS);
                return "returned:" + v;
            } catch (ExecutionException e) {
                Throwable cause = e.getCause();
                return "ExecutionException/"
                    + (cause == null ? "null" : cause.getClass().getName()
                        + ":" + cause.getMessage());
            } catch (Throwable t) {
                return t.getClass().getName() + ":" + t.getMessage();
            }
        }));
        row("invokeAll.futureIsDone.afterFailure", val(() -> {
            List<Future<Integer>> fs = pool.invokeAll(
                Arrays.<Callable<Integer>>asList(boom()));
            return fs.size() == 1 && fs.get(0).isDone();
        }));
        rowText("submit.Callable.onFailure", text(() -> {
            ForkJoinTask<Integer> t = pool.submit(boom());
            try {
                Integer v = t.get(30, TimeUnit.SECONDS);
                return "returned:" + v;
            } catch (ExecutionException e) {
                Throwable cause = e.getCause();
                return "ExecutionException/"
                    + (cause == null ? "null" : cause.getClass().getName());
            } catch (Throwable other) {
                return other.getClass().getName();
            }
        }));

        // --- future-object contract on invokeAll results -----------------
        row("invokeAll.future.isCancelled.false", val(() -> {
            List<Future<Integer>> fs = pool.invokeAll(callables(51));
            return !fs.get(0).isCancelled();
        }));
        row("invokeAll.future.cancel.afterDone.false", val(() -> {
            List<Future<Integer>> fs = pool.invokeAll(callables(52));
            return !fs.get(0).cancel(true);
        }));
        row("invokeAll.future.get.noTimeout", val(() -> {
            List<Future<Integer>> fs = pool.invokeAll(callables(53));
            return fs.get(0).get() == 53;
        }));
        row("invokeAll.result.isList", val(() -> {
            Object o = pool.invokeAll(callables(54));
            return o instanceof List;
        }));

        // --- a non-collection Collection receiver: Weld passes an
        // --- ArrayList, but a Set / unmodifiable view must work too ------
        row("invokeAll.setReceiver", val(() -> {
            Collection<Callable<Integer>> set =
                new java.util.LinkedHashSet<>(callables(61, 62));
            List<Future<Integer>> fs = pool.invokeAll(set);
            return fs.size() == 2 && sum(fs) == 123;
        }));
        row("invokeAll.unmodifiableReceiver", val(() -> {
            List<Future<Integer>> fs =
                pool.invokeAll(Collections.unmodifiableList(callables(63, 64)));
            return fs.size() == 2 && sum(fs) == 127;
        }));

        // --- nested submission: Weld's deployer submits from inside tasks -
        row("invokeAll.nested", val(() -> {
            List<Future<Integer>> outer = pool.invokeAll(
                Collections.<Callable<Integer>>singletonList(() -> {
                    List<Future<Integer>> inner = ForkJoinPool.commonPool()
                        .invokeAll(callables(71, 72));
                    return sum(inner);
                }));
            return outer.size() == 1 && outer.get(0).get(30, TimeUnit.SECONDS) == 143;
        }));

        // --- an explicitly constructed (non-common) pool -----------------
        row("newPool.invokeAll", val(() -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                List<Future<Integer>> fs = p.invokeAll(callables(81, 82));
                return fs.size() == 2 && sum(fs) == 163;
            } finally {
                p.shutdown();
            }
        }));
        row("newPool.submit", val(() -> {
            ForkJoinPool p = new ForkJoinPool(2);
            try {
                return p.submit(callable(83)).get(30, TimeUnit.SECONDS) == 83;
            } finally {
                p.shutdown();
            }
        }));

        // --- shutdown surfaces on the COMMON pool: the JDK contract is
        // --- that they are no-ops (the common pool is never shut down) ---
        row("commonPool.shutdown.isNoOp", val(() -> {
            pool.shutdown();
            return !pool.isShutdown() && !pool.isTerminated();
        }));
        row("commonPool.stillUsableAfterShutdown", val(() ->
            pool.submit(callable(91)).get(30, TimeUnit.SECONDS) == 91));
        row("commonPool.shutdownNow.emptyList", val(() -> {
            List<Runnable> left = pool.shutdownNow();
            return left != null && left.isEmpty() && !pool.isShutdown();
        }));
        row("commonPool.awaitTermination.false", val(() ->
            !pool.awaitTermination(1, TimeUnit.MILLISECONDS)));

        // --- static managedBlock -----------------------------------------
        row("managedBlock.ok", val(() -> {
            final boolean[] released = { false };
            ForkJoinPool.managedBlock(new ForkJoinPool.ManagedBlocker() {
                @Override
                public boolean block() {
                    released[0] = true;
                    return true;
                }

                @Override
                public boolean isReleasable() {
                    return released[0];
                }
            });
            return released[0];
        }));

        System.out.println("RAN_TOTAL " + RAN.get());
        System.out.println("FJP_MATRIX_DONE");
    }

    // ---------------------------------------------------------------- utils

    interface Body {
        boolean run() throws Throwable;
    }

    interface TextBody {
        String run() throws Throwable;
    }

    /** Same as {@link #val}, for rows whose contract is a shape, not a bool. */
    static Object text(TextBody b) {
        try {
            return b.run();
        } catch (Throwable t) {
            return t;
        }
    }

    static void rowText(String name, Object result) {
        row(name, result);
    }

    /** Runs `b`, converting any throwable into a printable marker. */
    static Object val(Body b) {
        try {
            return b.run();
        } catch (Throwable t) {
            return t;
        }
    }

    static void row(String name, Object result) {
        if (result instanceof Throwable) {
            Throwable t = (Throwable) result;
            String msg = t.getMessage();
            System.out.println("ROW " + name + " EXC " + t.getClass().getName()
                + (msg == null ? "" : " (" + msg + ")"));
        } else {
            System.out.println("ROW " + name + " = " + result);
        }
    }

    static Callable<Integer> callable(final int v) {
        return () -> {
            RAN.incrementAndGet();
            return v;
        };
    }

    static Callable<Integer> boom() {
        return () -> {
            RAN.incrementAndGet();
            throw new IllegalStateException("boom");
        };
    }

    static Runnable runnable() {
        return RAN::incrementAndGet;
    }

    static List<Callable<Integer>> callables(int... vs) {
        List<Callable<Integer>> out = new ArrayList<>();
        for (int v : vs) {
            out.add(callable(v));
        }
        return out;
    }

    /** Sleeps before bumping RAN, so an immediate "quiescent" answer loses. */
    static Runnable slowRunnable() {
        return () -> {
            try {
                Thread.sleep(300);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            RAN.incrementAndGet();
        };
    }

    static ForkJoinTask<Integer> throwingTask() {
        return new RecursiveTask<Integer>() {
            @Override
            protected Integer compute() {
                RAN.incrementAndGet();
                throw new IllegalStateException("task-boom");
            }
        };
    }

    static ForkJoinTask<Integer> task(final int v) {
        return new RecursiveTask<Integer>() {
            @Override
            protected Integer compute() {
                RAN.incrementAndGet();
                return v;
            }
        };
    }

    static boolean allDone(List<Future<Integer>> fs) {
        for (Future<Integer> f : fs) {
            if (!f.isDone()) {
                return false;
            }
        }
        return true;
    }

    static int sum(List<Future<Integer>> fs) throws Exception {
        int total = 0;
        for (Future<Integer> f : fs) {
            total += f.get(30, TimeUnit.SECONDS);
        }
        return total;
    }
}
