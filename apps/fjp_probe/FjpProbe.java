// RFJP.1 — divide-and-conquer Long sum via ForkJoinPool/RecursiveTask.
//
// Sum of 0..999_999 = 499_999_500_000.
//
// Pre-fix this returned 0 (real-JDK mode JIT correctness regression on
// deeply-recursive compute() at depth >= 10). Workaround applied in
// vm/src/runtime/interpreter.rs: refuse to JIT-compile any method whose
// declaring class extends ForkJoinTask / RecursiveTask / RecursiveAction
// / CountedCompleter, forcing interpreter execution for those methods.
//
// Expected on success:
//   sum = 499999500000
//   OK

import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveTask;

public class FjpProbe {

    static final int N = 1_000_000;
    static final int THRESHOLD = 1_000;

    public static class Sum extends RecursiveTask<Long> {
        final long[] a;
        final int lo;
        final int hi;

        Sum(long[] a, int lo, int hi) {
            this.a = a;
            this.lo = lo;
            this.hi = hi;
        }

        @Override
        protected Long compute() {
            int len = hi - lo;
            if (len <= THRESHOLD) {
                long s = 0L;
                for (int i = lo; i < hi; i++) {
                    s += a[i];
                }
                return Long.valueOf(s);
            }
            int mid = lo + (len >>> 1);
            Sum left = new Sum(a, lo, mid);
            Sum right = new Sum(a, mid, hi);
            left.fork();
            long r = right.compute().longValue();
            long l = left.join().longValue();
            return Long.valueOf(l + r);
        }
    }

    public static void main(String[] args) {
        long[] a = new long[N];
        for (int i = 0; i < N; i++) {
            a[i] = (long) i;
        }
        ForkJoinPool pool = new ForkJoinPool();
        long sum = pool.invoke(new Sum(a, 0, N)).longValue();
        System.out.println("sum = " + sum);
        if (sum == 499999500000L) {
            System.out.println("OK");
        } else {
            System.out.println("FAIL expected=499999500000 got=" + sum);
        }
    }
}
