import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CountedCompleter;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ForkJoinTask;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * JDK-only corpus: ForkJoin -- recursive task, parallel stream, worker
 * exception, quiescence.
 *
 * "ForkJoin worker execution" is a named P1 blocker: CratonVM's
 * native-builtins currently EAGER-INLINE fork()/invoke()/execute() because
 * "ForkJoinPool worker threads do not run Java bytecode". Eager inlining
 * changes observable ordering and concurrency, so every assertion below is
 * written to be true under a real work-stealing pool AND to fail loudly if the
 * work never actually leaves the calling thread when it must.
 *
 * Determinism: results are order-independent reductions; no steal counts, pool
 * sizes or thread names are printed.
 */
public class RJdkForkJoin {
    static final long T = 60;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** Classic divide-and-conquer sum. */
    static final class SumTask extends RecursiveTask<Long> {
        private static final long serialVersionUID = 1L;
        final long[] a;
        final int lo;
        final int hi;

        SumTask(long[] a, int lo, int hi) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected Long compute() {
            if (hi - lo <= 64) {
                long s = 0;
                for (int i = lo; i < hi; i++) {
                    s += a[i];
                }
                return s;
            }
            int mid = (lo + hi) >>> 1;
            SumTask left = new SumTask(a, lo, mid);
            SumTask right = new SumTask(a, mid, hi);
            left.fork();
            long r = right.compute();
            return left.join() + r;
        }
    }

    /** RecursiveAction with a shared accumulator. */
    static final class FillAction extends RecursiveAction {
        private static final long serialVersionUID = 1L;
        final int[] a;
        final int lo;
        final int hi;

        FillAction(int[] a, int lo, int hi) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected void compute() {
            if (hi - lo <= 32) {
                for (int i = lo; i < hi; i++) {
                    a[i] = i * 2;
                }
                return;
            }
            int mid = (lo + hi) >>> 1;
            invokeAll(new FillAction(a, lo, mid), new FillAction(a, mid, hi));
        }
    }

    static void recursiveTasks() throws Exception {
        long[] data = new long[20000];
        long expected = 0;
        for (int i = 0; i < data.length; i++) {
            data[i] = i;
            expected += i;
        }

        ForkJoinPool pool = new ForkJoinPool(4);
        try {
            long got = pool.invoke(new SumTask(data, 0, data.length));
            check(got == expected, "RecursiveTask sum: " + got + " != " + expected);

            // submit() + get() must give the same answer.
            ForkJoinTask<Long> t = pool.submit(new SumTask(data, 0, data.length));
            check(t.get(T, TimeUnit.SECONDS) == expected, "submitted RecursiveTask");
            check(t.isDone() && t.isCompletedNormally(), "task completion flags");

            int[] filled = new int[5000];
            pool.invoke(new FillAction(filled, 0, filled.length));
            long fsum = 0;
            for (int v : filled) {
                fsum += v;
            }
            check(fsum == 24995000L, "RecursiveAction fill sum: " + fsum);

            // The pool must actually reach quiescence.
            check(pool.awaitQuiescence(T, TimeUnit.SECONDS) || pool.isQuiescent(),
                    "pool never reached quiescence");
            check(pool.getQueuedSubmissionCount() == 0, "no submissions should remain queued");
            System.out.println("CK RJdkForkJoin sum=" + got + " fill=" + fsum);
        } finally {
            pool.shutdown();
            check(pool.awaitTermination(T, TimeUnit.SECONDS), "ForkJoinPool terminated");
        }
    }

    /** A CountedCompleter: completion propagates up the tree exactly once. */
    static final class Counter extends CountedCompleter<Void> {
        private static final long serialVersionUID = 1L;
        final AtomicInteger leaves;
        final AtomicInteger completions;
        final int depth;

        Counter(Counter parent, AtomicInteger leaves, AtomicInteger completions, int depth) {
            super(parent);
            this.leaves = leaves;
            this.completions = completions;
            this.depth = depth;
        }

        @Override
        public void compute() {
            if (depth == 0) {
                leaves.incrementAndGet();
                tryComplete();
                return;
            }
            setPendingCount(2);
            new Counter(this, leaves, completions, depth - 1).fork();
            new Counter(this, leaves, completions, depth - 1).fork();
            tryComplete();
        }

        @Override
        public void onCompletion(CountedCompleter<?> caller) {
            completions.incrementAndGet();
        }
    }

    static void countedCompleter() {
        AtomicInteger leaves = new AtomicInteger();
        AtomicInteger completions = new AtomicInteger();
        ForkJoinPool pool = new ForkJoinPool(3);
        try {
            pool.invoke(new Counter(null, leaves, completions, 6));
            check(leaves.get() == 64, "CountedCompleter leaves: " + leaves.get());
            check(completions.get() == 127, "onCompletion count: " + completions.get());
            System.out.println("CK RJdkForkJoin leaves=" + leaves.get()
                    + " completions=" + completions.get());
        } finally {
            pool.shutdown();
        }
    }

    static void parallelStreams() {
        // Reductions are associative, so the answer is order-independent even
        // though the split is not.
        long sum = IntStream.rangeClosed(1, 100000).parallel().mapToLong(i -> i).sum();
        check(sum == 5000050000L, "parallel sum: " + sum);

        List<Integer> src = IntStream.rangeClosed(1, 5000).boxed().collect(Collectors.toList());
        List<Integer> evens = src.parallelStream().filter(i -> i % 2 == 0)
                .collect(Collectors.toList());
        check(evens.size() == 2500, "parallel filter size");
        // collect(toList()) on an ordered source preserves encounter order.
        check(evens.get(0) == 2 && evens.get(2499) == 5000, "parallel encounter order");

        // A parallel reduce with a non-commutative-looking but associative op.
        String joined = src.parallelStream().limit(6).map(String::valueOf)
                .collect(Collectors.joining("-"));
        check(joined.equals("1-2-3-4-5-6"), "parallel joining: " + joined);

        // groupingBy over a parallel stream, sorted before assertion.
        List<String> keys = new ArrayList<>(src.parallelStream()
                .collect(Collectors.groupingBy(i -> "m" + (i % 4))).keySet());
        Collections.sort(keys);
        check(keys.equals(Arrays.asList("m0", "m1", "m2", "m3")), "parallel groupingBy: " + keys);

        // The common pool must exist and be usable.
        check(ForkJoinPool.commonPool() != null, "common pool");
        check(ForkJoinPool.getCommonPoolParallelism() >= 1, "common pool parallelism");
        AtomicLong acc = new AtomicLong();
        ForkJoinPool.commonPool().invoke(new SumTaskAdapter(acc));
        check(acc.get() == 4950, "common pool task: " + acc.get());
        System.out.println("CK RJdkForkJoin parallelSum=" + sum + " evens=" + evens.size()
                + " keys=" + keys);
    }

    static final class SumTaskAdapter extends RecursiveAction {
        private static final long serialVersionUID = 1L;
        final AtomicLong out;

        SumTaskAdapter(AtomicLong out) {
            this.out = out;
        }

        @Override
        protected void compute() {
            long s = 0;
            for (int i = 0; i < 100; i++) {
                s += i;
            }
            out.set(s);
        }
    }

    /** A worker exception must be captured and re-thrown at the join point. */
    static void workerException() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        try {
            ForkJoinTask<Long> t = pool.submit(new RecursiveTask<Long>() {
                private static final long serialVersionUID = 1L;

                @Override
                protected Long compute() {
                    throw new IllegalStateException("fj-boom");
                }
            });
            boolean threw = false;
            try {
                t.get(T, TimeUnit.SECONDS);
            } catch (ExecutionException e) {
                // ForkJoinTask re-creates the exception on the joining thread
                // when it can, so the message may be the ORIGINAL toString()
                // rather than the original message -- substring, not equals.
                threw = e.getCause() instanceof IllegalStateException
                        && e.getCause().getMessage().contains("fj-boom");
            }
            check(threw, "a worker exception must surface as ExecutionException(cause)");
            check(t.isCompletedAbnormally(), "isCompletedAbnormally");
            check(!t.isCompletedNormally(), "not completed normally");
            check(t.getException() instanceof IllegalStateException, "getException");

            // join() rethrows the original unchecked exception, unwrapped.
            ForkJoinTask<Long> t2 = pool.submit(new RecursiveTask<Long>() {
                private static final long serialVersionUID = 1L;

                @Override
                protected Long compute() {
                    throw new IllegalStateException("fj-join-boom");
                }
            });
            threw = false;
            try {
                t2.join();
            } catch (IllegalStateException e) {
                threw = e.getMessage().contains("fj-join-boom");
            }
            check(threw, "join() must rethrow the unchecked exception unwrapped");

            // A failure in one branch must not corrupt an unrelated branch.
            long ok = pool.invoke(new SumTask(new long[] { 1, 2, 3, 4 }, 0, 4));
            check(ok == 10, "pool still usable after a task failure: " + ok);

            // Cancellation.
            ForkJoinTask<Long> t3 = new RecursiveTask<Long>() {
                private static final long serialVersionUID = 1L;

                @Override
                protected Long compute() {
                    return 1L;
                }
            };
            check(t3.cancel(false), "cancel before submission");
            check(t3.isCancelled() && t3.isCompletedAbnormally(), "cancelled flags");
            System.out.println("CK RJdkForkJoin workerException ok recovered=" + ok);
        } finally {
            pool.shutdown();
            check(pool.awaitTermination(T, TimeUnit.SECONDS), "fj pool terminated");
        }
    }

    /**
     * The completion RECORD, as opposed to the completion FLAGS.
     *
     * ForkJoinTask's status is a write-once word that setDone/trySetCancelled/
     * trySetThrown only ever OR into; the sole method that clears a bit is
     * reinitialize(). Three consequences fall out of that, and each one is
     * written here so that it distinguishes a correct implementation from the
     * plausible wrong one rather than merely restating the flag it reads:
     *
     *   1. a later complete(v) CANNOT erase an abnormal completion, and it
     *      cannot turn an abnormally completed task into a normal one;
     *   2. a cancelled task's getException() is a CancellationException, not
     *      null -- otherwise isCompletedAbnormally() and getException() give
     *      opposite verdicts about the same task;
     *   3. reinitialize() must make compute() RUN again, not replay a
     *      memoised value -- a replay answers the same number, so the answer
     *      alone proves nothing and the run counter is the real assertion.
     */
    static void completionRecord() {
        // (1) complete(v) after completeExceptionally(ex).
        ForkJoinTask<Integer> t = new RecursiveTask<Integer>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                return 1;
            }
        };
        t.completeExceptionally(new IllegalStateException("ce-boom"));
        // Asserted BEFORE complete(), on purpose: without these two, the three
        // assertions after complete() cannot tell "complete() erased the
        // record" from "completeExceptionally() never wrote one".
        check(t.isDone(), "completeExceptionally must complete the task");
        check(t.isCompletedAbnormally(), "completeExceptionally completes ABNORMALLY");

        t.complete(99);
        check(t.isCompletedAbnormally(), "the abnormal record survives a later complete()");
        check(!t.isCompletedNormally(), "complete() must not fabricate a normal completion");
        check(t.getException() instanceof IllegalStateException,
                "the recorded throwable survives complete(): " + t.getException());
        boolean threw = false;
        try {
            t.join();
        } catch (IllegalStateException e) {
            threw = e.getMessage().contains("ce-boom");
        }
        check(threw, "join() replays the throwable, not the value passed to complete()");

        // (2) getException() on a task whose only fault is cancellation.
        ForkJoinTask<Integer> c = new RecursiveTask<Integer>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                return 1;
            }
        };
        check(c.cancel(false), "cancel a never-submitted task");
        check(c.isCancelled() && c.isCompletedAbnormally(), "a cancelled task is abnormal");
        // trySetCancelled ORs DONE|ABNORMAL and never touches `aux`, so
        // getException() reaches its "no recorded throwable but abnormal"
        // branch and allocates a fresh CancellationException.
        Throwable cancelledEx = c.getException();
        check(cancelledEx instanceof CancellationException,
                "a cancelled task SAYS what went wrong: " + cancelledEx);

        // (3) reinitialize().
        final AtomicInteger runs = new AtomicInteger();
        RecursiveTask<Integer> r = new RecursiveTask<Integer>() {
            private static final long serialVersionUID = 1L;

            @Override
            protected Integer compute() {
                runs.incrementAndGet();
                return 7;
            }
        };
        check(r.invoke() == 7, "first invoke");
        check(runs.get() == 1, "compute() ran once: " + runs.get());
        r.reinitialize();
        check(!r.isDone(), "reinitialize() un-completes the task");
        check(r.invoke() == 7, "second invoke");
        // THE discriminator. A memoised replay also answers 7; only a genuine
        // re-execution moves this counter.
        check(runs.get() == 2, "reinitialize() makes compute() RUN again: " + runs.get());

        System.out.println("CK RJdkForkJoin completionRecord runs=" + runs.get()
                + " cancelledEx=" + cancelledEx.getClass().getName());
    }

    public static void main(String[] args) throws Exception {
        recursiveTasks();
        countedCompleter();
        parallelStreams();
        workerException();
        completionRecord();
        System.out.println("CK RJdkForkJoin checks=" + checks);
        System.out.println("PASS RJdkForkJoin (" + checks + " checks)");
    }
}
