import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountedCompleter;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * L5 -- "a forked task runs exactly once", asked three ways.
 *
 * This probe exists because the number that identified the defect is a COUNT of
 * executions, not a result: a divide-and-conquer SUM is idempotent, so a task
 * executed twice still sums correctly and every ordinary ForkJoin assertion
 * passes over a pool that is running everything twice. `RJdkForkJoin`'s
 * `CountedCompleter leaves: 128` is the one assertion in the corpus that
 * counts SIDE EFFECTS instead of reducing values, which is why it was the only
 * vector that saw it.
 *
 * Every number printed here is a count of compute() entries against a tree
 * whose shape is fixed by construction, so nothing here depends on the number
 * of workers, on steal order, or on the host's CPU count. No thread names, no
 * pool sizes, no timings.
 */
public class L5CountedCompleter {
    static final class Counter extends CountedCompleter<Void> {
        private static final long serialVersionUID = 1L;
        final AtomicInteger leaves;
        final AtomicInteger completions;
        final AtomicInteger computes;
        final int depth;

        Counter(Counter parent, AtomicInteger leaves, AtomicInteger completions,
                AtomicInteger computes, int depth) {
            super(parent);
            this.leaves = leaves;
            this.completions = completions;
            this.computes = computes;
            this.depth = depth;
        }

        @Override
        public void compute() {
            computes.incrementAndGet();
            if (depth == 0) {
                leaves.incrementAndGet();
                tryComplete();
                return;
            }
            setPendingCount(2);
            new Counter(this, leaves, completions, computes, depth - 1).fork();
            new Counter(this, leaves, completions, computes, depth - 1).fork();
            tryComplete();
        }

        @Override
        public void onCompletion(CountedCompleter<?> caller) {
            completions.incrementAndGet();
        }
    }

    /** A leaf whose only observable is that it ran. */
    static final class Once extends RecursiveAction {
        private static final long serialVersionUID = 1L;
        final AtomicInteger runs;

        Once(AtomicInteger runs) {
            this.runs = runs;
        }

        @Override
        public void compute() {
            runs.incrementAndGet();
        }
    }

    /** Forks n leaves and joins them all. Uses a local list, filled once. */
    static final class Fan extends RecursiveAction {
        private static final long serialVersionUID = 1L;
        final int n;
        final AtomicInteger runs;
        final AtomicInteger selfComputes;

        Fan(int n, AtomicInteger runs, AtomicInteger selfComputes) {
            this.n = n;
            this.runs = runs;
            this.selfComputes = selfComputes;
        }

        @Override
        public void compute() {
            selfComputes.incrementAndGet();
            List<Once> kids = new ArrayList<>();
            for (int i = 0; i < n; i++) {
                Once o = new Once(runs);
                kids.add(o);
                o.fork();
            }
            for (Once o : kids) {
                o.join();
            }
        }
    }

    static void ccTree(int depth, int expectedLeaves, int expectedNodes) {
        AtomicInteger leaves = new AtomicInteger();
        AtomicInteger completions = new AtomicInteger();
        AtomicInteger computes = new AtomicInteger();
        ForkJoinPool pool = new ForkJoinPool(3);
        try {
            pool.invoke(new Counter(null, leaves, completions, computes, depth));
        } finally {
            pool.shutdown();
        }
        System.out.println("ccTree depth=" + depth
                + " leaves=" + leaves.get() + "/" + expectedLeaves
                + " completions=" + completions.get() + "/" + expectedNodes
                + " computes=" + computes.get() + "/" + expectedNodes);
    }

    static void fanOut(int n) throws Exception {
        AtomicInteger runs = new AtomicInteger();
        AtomicInteger selfComputes = new AtomicInteger();
        ForkJoinPool pool = new ForkJoinPool(3);
        pool.invoke(new Fan(n, runs, selfComputes));
        pool.shutdown();
        pool.awaitTermination(60, TimeUnit.SECONDS);
        System.out.println("fanOut n=" + n + " leafRuns=" + runs.get() + "/" + n
                + " rootComputes=" + selfComputes.get() + "/1");
    }

    static void invokeAllOnce(int n) throws Exception {
        AtomicInteger runs = new AtomicInteger();
        final List<Once> l = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            l.add(new Once(runs));
        }
        ForkJoinPool pool = new ForkJoinPool(3);
        pool.invoke(new RecursiveAction() {
            private static final long serialVersionUID = 1L;

            @Override
            public void compute() {
                invokeAll(l.toArray(new Once[0]));
            }
        });
        pool.shutdown();
        pool.awaitTermination(60, TimeUnit.SECONDS);
        System.out.println("invokeAll n=" + n + " runs=" + runs.get() + "/" + n);
    }

    public static void main(String[] args) throws Exception {
        int rows = 0;
        ccTree(1, 2, 3);
        rows++;
        ccTree(2, 4, 7);
        rows++;
        ccTree(3, 8, 15);
        rows++;
        ccTree(6, 64, 127);
        rows++;
        for (int t = 0; t < 3; t++) {
            fanOut(200);
            rows++;
        }
        invokeAllOnce(100);
        rows++;
        System.out.println("rows " + rows);
        System.out.println("DONE L5CountedCompleter");
    }
}
