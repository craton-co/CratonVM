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

    /**
     * The DEEP divide-and-conquer shape, kept separate from {@link SumTask}
     * because the depth is the point.
     *
     * {@code SumTask} splits 20 000 elements at a threshold of 64, i.e.
     * recursion depth ~log2(20000/64) ~= 9. RFJP.1 -- the JIT miscompile that
     * made every {@code ForkJoinTask} subclass force-interpreted for a year --
     * was recorded as appearing "once the recursion depth is ~10+", so that
     * vector sat just BELOW the failing depth and its pass proved nothing about
     * it. A threshold of 2 over 65 536 elements forces depth 15.
     *
     * Two properties are deliberate and must not be "simplified":
     *
     *   * the return type is {@code Long}, so every recursive return BOXES and
     *     every use UNBOXES -- RFJP.1 was root-caused to a {@code Long.valueOf}
     *     boxing miscompile, not to regalloc as its first diagnosis claimed;
     *   * {@code r} is a {@code long} local held live ACROSS the recursive
     *     {@code right.compute()} call, which is the operand-stack shape the
     *     original note describes.
     *
     * See `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.
     */
    static final class DeepSumTask extends RecursiveTask<Long> {
        private static final long serialVersionUID = 1L;
        final long[] a;
        final int lo;
        final int hi;
        final int depth;

        DeepSumTask(long[] a, int lo, int hi, int depth) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
            this.depth = depth;
        }

        @Override
        protected Long compute() {
            if (depth > MAX_DEPTH.get()) {
                MAX_DEPTH.accumulateAndGet(depth, Math::max);
            }
            if (hi - lo <= 2) {
                long s = 0;
                for (int i = lo; i < hi; i++) {
                    s += a[i];
                }
                return s;
            }
            int mid = (lo + hi) >>> 1;
            DeepSumTask left = new DeepSumTask(a, lo, mid, depth + 1);
            left.fork();
            DeepSumTask right = new DeepSumTask(a, mid, hi, depth + 1);
            long r = right.compute();
            return left.join() + r;
        }
    }

    static final AtomicInteger MAX_DEPTH = new AtomicInteger();

    /**
     * Deep recursion, run enough times that the JIT compiles {@code compute()}
     * and the answer is produced by COMPILED code rather than by the
     * interpreter.
     *
     * The rounds are not decoration. A single invoke can finish before the
     * invocation counter crosses the C1 threshold, and a green run in which
     * {@code compute()} was never compiled says nothing about a JIT defect.
     * The depth assertion is the other half: without it a mistyped threshold
     * turns this into a shallow tree that still sums correctly.
     */
    static void deepRecursion() {
        int n = 65536;
        long[] data = new long[n];
        long expected = 0;
        for (int i = 0; i < n; i++) {
            data[i] = i;
            expected += i;
        }
        ForkJoinPool pool = new ForkJoinPool(4);
        try {
            for (int round = 0; round < 3; round++) {
                long got = pool.invoke(new DeepSumTask(data, 0, n, 0));
                check(got == expected,
                        "deep RecursiveTask sum round " + round + ": " + got + " != " + expected);
            }
            check(MAX_DEPTH.get() >= 14,
                    "deep tree must recurse past the RFJP.1 depth; got " + MAX_DEPTH.get());
            System.out.println("CK RJdkForkJoin deepSum=" + expected + " depth=" + MAX_DEPTH.get());
        } finally {
            pool.shutdown();
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

        // Common-pool parallelism, as a NUMBER rather than as a sign check.
        //
        // This assertion used to be `getCommonPoolParallelism() >= 1`, which is
        // satisfied by the wrong answer -- a hardcoded 1, i.e. a common pool
        // with no parallelism at all -- exactly as well as by the right one. It
        // read green on every host and measured nothing at all. What replaces it
        // discriminates in three directions:
        //
        //   (a) The STATIC accessor and the pool's own INSTANCE accessor must
        //       answer the SAME number. The JDK specifies them as equal, so a
        //       disagreement means one of the two is fabricated independently of
        //       the pool it claims to describe.
        //   (b) With `...common.parallelism` unset -- which is how the suite runs
        //       -- the JDK's documented default is `max(1, availableProcessors()
        //       - 1)`. That is the HotSpot answer, and it is the only answer this
        //       check accepts unless the VM is taking the ONE named exception
        //       below.
        //   (c) The named exception, pinned to a single value rather than left
        //       open: CratonVM's common pool is a proxy that runs every task on
        //       the submitting thread, and its parallelism is deliberately
        //       clamped to 1 for determinism (a shared constant, so the static
        //       and instance readouts cannot drift). That is a KNOWN divergence
        //       from HotSpot on any host with 3+ CPUs, and pinning it here is
        //       the point: a THIRD answer -- 4 on an 8-CPU host, say, or 2 where
        //       the formula says 7 -- is a fabricated number and fails.
        //
        // What the CK line may print is constrained by the harness: run.sh
        // diffs CK lines between CratonVM and HotSpot in the same session, so
        // printing `par` itself would FAIL that diff on every host with 3+ CPUs
        // -- HotSpot answers max(1, procs-1) and CratonVM answers the clamp of
        // 1, which is exactly the divergence (c) exists to pin. The assertion
        // above is where the discrimination lives; the CK line publishes only
        // VM-INDEPENDENT facts: that the accepted-set test held, and whether
        // this host has one CPU. The second is what makes a run in which (b)
        // and (c) coincide visible as such instead of silently reading green.
        int procs = Runtime.getRuntime().availableProcessors();
        int par = ForkJoinPool.getCommonPoolParallelism();
        int poolPar = ForkJoinPool.commonPool().getParallelism();
        check(procs >= 1, "availableProcessors must be positive: " + procs);
        check(par == poolPar,
                "static and instance common-pool parallelism must agree: " + par + " vs " + poolPar);
        String parProp = System.getProperty("java.util.concurrent.ForkJoinPool.common.parallelism");
        if (parProp == null) {
            int hotspotDefault = Math.max(1, procs - 1);
            check(par == hotspotDefault || par == 1,
                    "common pool parallelism must be either the JDK default max(1, procs-1)="
                            + hotspotDefault + " or the documented inline-proxy clamp of 1; got "
                            + par + " with procs=" + procs);
        }
        AtomicLong acc = new AtomicLong();
        ForkJoinPool.commonPool().invoke(new SumTaskAdapter(acc));
        check(acc.get() == 4950, "common pool task: " + acc.get());
        System.out.println("CK RJdkForkJoin parallelSum=" + sum + " evens=" + evens.size()
                + " keys=" + keys + " commonParAccepted=true singleCpuHost=" + (procs == 1));
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
        deepRecursion();
        countedCompleter();
        parallelStreams();
        workerException();
        completionRecord();
        System.out.println("CK RJdkForkJoin checks=" + checks);
        System.out.println("PASS RJdkForkJoin (" + checks + " checks)");
    }
}
