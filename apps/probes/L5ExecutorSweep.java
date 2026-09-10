import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.AbstractExecutorService;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * L5 -- the executor and task-status surface, written to make the registry
 * DISPATCH every row of it.
 *
 * # Why this probe exists, which is not the usual reason
 *
 * A retirement needs four things (`jdk-only-lane-operations.md` §7), and the
 * fourth is a dispatch observed PER TRIPLE by the instrument whose improvement
 * is being cited. When lane 5 measured its clean classes, 84 of their rows had
 * `invocations: 0` in every probe run AND in all 132 `--jdk-only` corpus
 * reports -- not because they are wrong, but because nothing in the tree calls
 * them. An unmeasured row is not a retirable row, so this file calls them.
 *
 * # The two shapes that decide which triple a call lands on
 *
 * A dispatch door asks the registry about the DECLARING class of the resolved
 * method, and this family registers the same method on the abstract base AND
 * on each concrete subclass. So `new RecursiveTask<>(){...}.join()` resolves to
 * `RecursiveTask.join`, and `ForkJoinTask.join` -- a different registration,
 * with its own row -- is never reached by it. Every task-status row below is
 * therefore asked TWICE: once on a bare `ForkJoinTask` subclass (which
 * declares `exec`/`getRawResult`/`setRawResult` itself and inherits the rest
 * straight from `ForkJoinTask`), and once on `RecursiveTask`/`RecursiveAction`.
 *
 * # Determinism
 *
 * Nothing here prints a pool size, an active count, a thread name, a queue
 * depth, a timing, or any number a scheduler chooses -- with one deliberate
 * exception that is not one: `ThreadPoolExecutor`'s counters are read AFTER
 * `shutdown()` and a successful `awaitTermination()`, at which point
 * `getPoolSize()` and `getActiveCount()` are 0 and `getCompletedTaskCount()`
 * is the number of tasks submitted, on any correct implementation. Every
 * `Async` composition is joined before its value is read, and every task is a
 * pure function of its input.
 */
public class L5ExecutorSweep {
    static int rows;

    static void say(String label, Object value) {
        rows++;
        System.out.println(label + " " + value);
    }

    static String thrown(Callable<?> c) {
        try {
            Object v = c.call();
            return "ok:" + v;
        } catch (Throwable t) {
            Throwable r = t;
            return r.getClass().getName();
        }
    }

    // ---- a task whose declaring class for the status family IS ForkJoinTask
    static final class Bare extends ForkJoinTask<String> {
        private static final long serialVersionUID = 1L;
        String result;
        final AtomicInteger runs;
        final boolean thrower;

        Bare(AtomicInteger runs, boolean thrower) {
            this.runs = runs;
            this.thrower = thrower;
        }

        @Override
        public String getRawResult() {
            return result;
        }

        @Override
        protected void setRawResult(String v) {
            result = v;
        }

        @Override
        protected boolean exec() {
            runs.incrementAndGet();
            if (thrower) {
                throw new IllegalStateException("bare");
            }
            result = "done";
            return true;
        }
    }

    static void forkJoinTaskRows() throws Exception {
        AtomicInteger runs = new AtomicInteger();
        ForkJoinPool pool = new ForkJoinPool(2);

        Bare a = new Bare(runs, false);
        say("bare invoke", pool.invoke(a));
        say("bare isDone", a.isDone());
        say("bare isCancelled", a.isCancelled());
        say("bare isCompletedNormally", a.isCompletedNormally());
        say("bare isCompletedAbnormally", a.isCompletedAbnormally());
        say("bare getException", String.valueOf(a.getException()));
        say("bare join", a.join());
        say("bare get", a.get());
        say("bare getTimed", a.get(30, TimeUnit.SECONDS));
        say("bare cancelAfterDone", a.cancel(true));
        a.reinitialize();
        say("bare isDoneAfterReinitialize", a.isDone());

        Bare b = new Bare(runs, false);
        b.complete("forced");
        say("bare completeThenJoin", b.join() + " done=" + b.isDone()
                + " normal=" + b.isCompletedNormally());

        Bare c = new Bare(runs, false);
        c.completeExceptionally(new IllegalArgumentException("x"));
        say("bare completeExceptionally isDone", c.isDone() + " abnormal=" + c.isCompletedAbnormally()
                + " ex=" + c.getException().getClass().getName());
        say("bare joinAfterCompleteExceptionally", thrown(() -> c.join()));
        say("bare getAfterCompleteExceptionally", thrown(() -> c.get()));
        c.quietlyJoin();
        say("bare quietlyJoinAfterAbnormal", c.isDone());

        Bare d = new Bare(runs, false);
        say("bare cancelBeforeRun", d.cancel(true) + " cancelled=" + d.isCancelled()
                + " abnormal=" + d.isCompletedAbnormally());
        say("bare joinAfterCancel", thrown(() -> d.join()));

        Bare e = new Bare(runs, false);
        e.quietlyInvoke();
        say("bare quietlyInvoke", e.getRawResult() + " done=" + e.isDone());

        Bare f = new Bare(runs, true);
        f.quietlyInvoke();
        say("bare quietlyInvokeThrower", f.isCompletedAbnormally()
                + " ex=" + f.getException().getClass().getName());

        Bare g = new Bare(runs, false);
        pool.execute(g);
        say("bare quietlyJoinTimed", g.quietlyJoin(30, TimeUnit.SECONDS));
        Bare h = new Bare(runs, false);
        pool.execute(h);
        say("bare quietlyJoinUninterruptibly", h.quietlyJoinUninterruptibly(30, TimeUnit.SECONDS));

        Bare i = new Bare(runs, false);
        i.quietlyComplete();
        say("bare quietlyComplete", i.isDone() + " normal=" + i.isCompletedNormally());

        say("bare inForkJoinPool from main", ForkJoinTask.inForkJoinPool());
        say("bare getPool from main", String.valueOf(ForkJoinTask.getPool()));
        say("bare tryUnforkNeverForked", new Bare(runs, false).tryUnfork());

        pool.shutdown();
        say("forkJoinTaskRows terminated", pool.awaitTermination(60, TimeUnit.SECONDS));
    }

    static void recursiveRows() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        AtomicInteger n = new AtomicInteger();

        RecursiveTask<Integer> t = new RecursiveTask<>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                n.incrementAndGet();
                return 41 + 1;
            }
        };
        say("rt invoke", t.invoke());
        say("rt getRawResult", t.getRawResult());
        say("rt isDone", t.isDone());
        say("rt get", t.get());
        say("rt cancelAfterDone", t.cancel(true));
        say("rt getPool from main", String.valueOf(t.getPool()));
        say("rt inForkJoinPool from main", t.inForkJoinPool());
        t.reinitialize();
        say("rt isDoneAfterReinitialize", t.isDone());
        say("rt join after reinitialize+invoke", t.invoke());

        RecursiveTask<Integer> t2 = new RecursiveTask<>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                return 7;
            }
        };
        t2.complete(99);
        say("rt completeThenJoin", t2.join());
        RecursiveTask<Integer> t3 = new RecursiveTask<>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                return 7;
            }
        };
        t3.completeExceptionally(new IllegalStateException("rt"));
        say("rt completeExceptionally", thrown(() -> t3.join()));
        say("rt getExceptionType", t3.getException().getClass().getName());

        RecursiveAction ra = new RecursiveAction() {
            private static final long serialVersionUID = 1L;

            @Override
            protected void compute() {
                n.incrementAndGet();
            }
        };
        say("ra invoke", String.valueOf(ra.invoke()));
        say("ra getRawResult", String.valueOf(ra.getRawResult()));
        say("ra isDone", ra.isDone());
        say("ra getPool from main", String.valueOf(ra.getPool()));
        say("ra inForkJoinPool from main", ra.inForkJoinPool());
        ra.reinitialize();
        say("ra isDoneAfterReinitialize", ra.isDone());
        RecursiveAction ra2 = new RecursiveAction() {
            private static final long serialVersionUID = 1L;

            @Override
            protected void compute() {
                n.incrementAndGet();
            }
        };
        ra2.complete(null);
        say("ra completeThenIsDone", ra2.isDone());
        RecursiveAction ra3 = new RecursiveAction() {
            private static final long serialVersionUID = 1L;

            @Override
            protected void compute() {
                n.incrementAndGet();
            }
        };
        ra3.completeExceptionally(new IllegalStateException("ra"));
        say("ra completeExceptionally", thrown(() -> ra3.join()));

        pool.shutdown();
        say("recursiveRows terminated", pool.awaitTermination(60, TimeUnit.SECONDS));
    }

    static void executorRows() throws Exception {
        ThreadFactory tf = r -> {
            Thread th = new Thread(r);
            th.setDaemon(true);
            return th;
        };
        ThreadPoolExecutor tpe = new ThreadPoolExecutor(
                2, 2, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>(), tf);
        AtomicInteger done = new AtomicInteger();
        List<Future<Integer>> fs = new ArrayList<>();
        for (int i = 0; i < 8; i++) {
            final int k = i;
            fs.add(tpe.submit(() -> {
                done.incrementAndGet();
                return k * 2;
            }));
        }
        int sum = 0;
        for (Future<Integer> f : fs) {
            sum += f.get();
        }
        say("tpe submitCallable sum", sum);
        Future<?> r1 = tpe.submit((Runnable) done::incrementAndGet);
        say("tpe submitRunnable value", String.valueOf(r1.get()));
        Future<String> r2 = tpe.submit(done::incrementAndGet, "fixed");
        say("tpe submitRunnableWithValue", r2.get());
        List<Callable<String>> any = Arrays.asList(() -> "only");
        say("tpe invokeAny", tpe.invokeAny(any));
        say("tpe corePoolSize", tpe.getCorePoolSize());
        tpe.shutdown();
        say("tpe terminated", tpe.awaitTermination(60, TimeUnit.SECONDS));
        // After a completed shutdown these are all determined.
        say("tpe activeCountAfterTermination", tpe.getActiveCount());
        say("tpe poolSizeAfterTermination", tpe.getPoolSize());
        say("tpe completedTaskCountAfterTermination", tpe.getCompletedTaskCount());
        say("tpe isShutdown", tpe.isShutdown() + " terminated=" + tpe.isTerminated());
        say("tpe submitAfterShutdown", thrown(() -> tpe.submit(() -> 1)));

        ScheduledThreadPoolExecutor st = new ScheduledThreadPoolExecutor(
                3, tf, new ThreadPoolExecutor.AbortPolicy());
        say("stpe corePoolSize", st.getCorePoolSize());
        say("stpe scheduledValue", st.schedule(() -> 5, 1, TimeUnit.MILLISECONDS).get());
        st.shutdown();
        say("stpe terminated", st.awaitTermination(60, TimeUnit.SECONDS));

        // AbstractExecutorService's own rows, on a subclass that adds nothing.
        ExecutorService aes = new ForkJoinPool(2);
        say("aes submitCallable", aes.submit(() -> "c").get());
        say("aes submitRunnable", String.valueOf(aes.submit((Runnable) () -> { }).get()));
        say("aes submitRunnableWithValue", aes.submit(() -> { }, "v").get());
        say("aes invokeAny", aes.invokeAny(Arrays.asList(() -> "a")));
        aes.shutdown();
        say("aes terminated", aes.awaitTermination(60, TimeUnit.SECONDS));
    }

    static void queueRows() throws Exception {
        PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
        say("pbq emptyNew", q.isEmpty() + " size=" + q.size() + " peek=" + q.peek());
        q.put(5);
        q.put(1);
        q.put(3);
        say("pbq afterPut", q.isEmpty() + " size=" + q.size() + " peek=" + q.peek());
        say("pbq take", q.take() + "," + q.take());
        say("pbq pollTimedHit", q.poll(5, TimeUnit.SECONDS));
        say("pbq pollTimedMiss", String.valueOf(q.poll(1, TimeUnit.MILLISECONDS)));
        say("pbq emptyAgain", q.isEmpty() + " peek=" + q.peek());
        say("pbq putNull", thrown(() -> {
            q.put(null);
            return "no-throw";
        }));
        say("pbq offerNull", thrown(() -> q.offer(null)));
    }

    static void futureRows() throws Exception {
        CompletableFuture<Integer> base = CompletableFuture.completedFuture(10);
        say("cf thenApplyAsync", base.thenApplyAsync(v -> v + 1).join());
        AtomicInteger seen = new AtomicInteger();
        say("cf thenAcceptAsync", String.valueOf(base.thenAcceptAsync(seen::set).join())
                + " seen=" + seen.get());
        say("cf thenRunAsync", String.valueOf(base.thenRunAsync(() -> seen.set(99)).join())
                + " seen=" + seen.get());
        say("cf thenComposeAsync",
                base.thenComposeAsync(v -> CompletableFuture.completedFuture(v * 3)).join());
        CompletableFuture<Integer> bad = new CompletableFuture<>();
        say("cf completeExceptionally", bad.completeExceptionally(new IllegalStateException("cf")));
        say("cf completeExceptionallyTwice",
                bad.completeExceptionally(new IllegalStateException("again")));
        say("cf joinAfterExceptional", thrown(() -> bad.join()));
        say("cf isCompletedExceptionally", bad.isCompletedExceptionally()
                + " done=" + bad.isDone() + " cancelled=" + bad.isCancelled());
    }

    static void timeUnitRows() {
        long ms = 90061000L;
        say("tu toDays", TimeUnit.MILLISECONDS.toDays(ms));
        say("tu toHours", TimeUnit.MILLISECONDS.toHours(ms));
        say("tu toMinutes", TimeUnit.MILLISECONDS.toMinutes(ms));
        say("tu toSeconds", TimeUnit.MILLISECONDS.toSeconds(ms));
        say("tu toMicros", TimeUnit.MILLISECONDS.toMicros(ms));
        say("tu toNanos", TimeUnit.MILLISECONDS.toNanos(ms));
        say("tu toMillis", TimeUnit.SECONDS.toMillis(90L));
        say("tu convert", TimeUnit.SECONDS.convert(1500L, TimeUnit.MILLISECONDS));
        // Saturation, not overflow: the JDK clamps rather than wrapping.
        say("tu toNanosSaturatesHigh", TimeUnit.DAYS.toNanos(Long.MAX_VALUE));
        say("tu toNanosSaturatesLow", TimeUnit.DAYS.toNanos(Long.MIN_VALUE));
        say("tu toMicrosSaturates", TimeUnit.DAYS.toMicros(Long.MAX_VALUE));
    }

    static void threadEnumRows() {
        say("threadState values", Arrays.toString(Thread.State.values()));
        say("threadState valueOf", Thread.State.valueOf("RUNNABLE"));
        say("threadState valueOfBad", thrown(() -> Thread.State.valueOf("NOPE")));
        Thread t = new Thread(() -> { });
        say("threadState NEW", t.getState());
    }

    public static void main(String[] args) throws Exception {
        forkJoinTaskRows();
        recursiveRows();
        executorRows();
        queueRows();
        futureRows();
        timeUnitRows();
        threadEnumRows();
        System.out.println("rows " + rows);
        System.out.println("DONE L5ExecutorSweep");
    }
}
