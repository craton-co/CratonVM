import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * RFJP.1 / WP4.3 acceptance probe — deep {@code RecursiveTask} recursion.
 *
 * <h2>Why this file exists (again)</h2>
 *
 * This fixture is referenced by {@code vm/tests/fjp_recursive.rs},
 * {@code vm/tests/rfjp1_recursive.rs}, {@code vm/src/threading/forkjoin.rs} and
 * several comments in {@code native-builtins/src/phases_early.rs} — but it was
 * ABSENT from the tree. Both Rust tests took their "probe source missing;
 * skipping" branch and reported {@code ok} in 0.00 s, so every claim below had
 * no live guard at all. Restored 2026-08-07 by the vacuous-test sweep, together
 * with a hard failure in both tests for the case where it goes missing again.
 *
 * <h2>What it pins</h2>
 *
 * A divide-and-conquer sum of {@code long[0..1_000_000)} with a 1000-element
 * threshold. That is the CALIBRATED shape the historical findings were recorded
 * against — 1000 leaves, recursion depth exactly 10 — so do not "simplify" the
 * constants:
 *
 * <ul>
 *   <li><b>RFJP.1 (JIT).</b> The JIT mis-compiled the recursive
 *       {@code compute()}-returning-{@code Long}; at depth &ge; 10 the probe
 *       printed {@code sum = 0}. Root-caused to a {@code Long.valueOf} boxing
 *       miscompile. The unboxing on {@code right.compute()} / {@code left.join()}
 *       below is deliberately on the hot path.</li>
 *   <li><b>WP4.3 (interpreter).</b> {@code lastore} on a {@code long[]} lost the
 *       value tag and zeroed the array. Hence a {@code long[]}, not an
 *       {@code int[]}.</li>
 *   <li><b>Lazy fork (threading).</b> {@code fork()} is called for real here.
 *       The recorded justification for lazy fork is that EAGER fork — running
 *       {@code compute()} inline at {@code fork()} time — Rust-recursed through
 *       {@code invoke_or_native} and overflowed the worker's host stack at this
 *       exact depth. As of 2026-08-07 the default is
 *       {@code FjtForkMode::CountedCompleterEager}, which a
 *       {@code RecursiveTask} receiver does NOT match, so the shipped default
 *       takes the lazy branch here. The always-eager arm is one command:
 *       {@code CRATONVM_FJP_EAGER_FORK=all cratonvm --java-home <jdk> -c
 *       <classes> FjpProbe}.</li>
 * </ul>
 *
 * <h2>Contract with the Rust harnesses</h2>
 *
 * On success prints {@code sum = 499999500000}, a {@code depth = 10} line, and
 * {@code OK}, and exits 0. On any failure it prints a {@code FAIL ...} line and
 * exits 1 — so a harness asserting {@code status.success()} is meaningful and a
 * silent wrong answer is impossible. Keeps the class name {@code FjpProbe} and
 * the nested task name {@code SumTask}: both harnesses gate on
 * {@code FjpProbe$SumTask.class} existing.
 *
 * The depth self-check is what stops THIS file from going vacuous: without it,
 * a mistyped threshold could make the probe a single non-recursive leaf that
 * still prints the right sum.
 */
public class FjpProbe {

    /** Element count. 0..N-1 sums to 499999500000. */
    static final int N = 1000000;

    /** Leaf size. N/THRESHOLD = 1000 leaves, i.e. recursion depth 10. */
    static final int THRESHOLD = 1000;

    /** sum(0 .. N-1) = (N-1)*N/2. */
    static final long EXPECTED_SUM = 499999500000L;

    /** The recursion must actually happen. Depth is 10; demand at least 8. */
    static final int MIN_DEPTH = 8;

    /** Deepest {@code compute()} entry observed, root = 0. */
    static final AtomicInteger MAX_DEPTH = new AtomicInteger();

    /** CAS-max. Written without lambdas so the probe leans on as little as possible. */
    static void recordDepth(int d) {
        int cur;
        while (d > (cur = MAX_DEPTH.get())) {
            if (MAX_DEPTH.compareAndSet(cur, d)) {
                return;
            }
        }
    }

    static final class SumTask extends RecursiveTask<Long> {
        private static final long serialVersionUID = 1L;

        private final long[] a;
        private final int lo;
        private final int hi;
        private final int depth;

        SumTask(long[] a, int lo, int hi, int depth) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
            this.depth = depth;
        }

        @Override
        protected Long compute() {
            recordDepth(depth);
            if (hi - lo <= THRESHOLD) {
                long s = 0;
                for (int i = lo; i < hi; i++) {
                    s += a[i];
                }
                return s;
            }
            int mid = (lo + hi) >>> 1;
            SumTask left = new SumTask(a, lo, mid, depth + 1);
            // The real fork: under an always-eager fork mode this is where the
            // host-stack recursion the lazy default was chosen to avoid begins.
            left.fork();
            SumTask right = new SumTask(a, mid, hi, depth + 1);
            // Both of these unbox a Long returned from a recursive call — the
            // RFJP.1 miscompile site.
            long r = right.compute();
            long l = left.join();
            return l + r;
        }
    }

    public static void main(String[] args) {
        long[] a = new long[N];
        for (int i = 0; i < N; i++) {
            a[i] = i;
        }

        ForkJoinPool pool = new ForkJoinPool(4);
        long sum;
        try {
            sum = pool.invoke(new SumTask(a, 0, N, 0));
        } finally {
            pool.shutdown();
        }

        int depth = MAX_DEPTH.get();
        System.out.println("sum = " + sum);
        System.out.println("depth = " + depth);

        if (sum != EXPECTED_SUM) {
            System.out.println("FAIL " + EXPECTED_SUM);
            System.exit(1);
        }
        if (depth < MIN_DEPTH) {
            // Not a VM bug — a broken probe. Say so, loudly, rather than
            // printing OK for a sum that never recursed.
            System.out.println("FAIL probe recursed to depth " + depth
                    + ", expected at least " + MIN_DEPTH
                    + "; N/THRESHOLD no longer produces a deep task tree");
            System.exit(1);
        }
        System.out.println("OK");
    }
}
