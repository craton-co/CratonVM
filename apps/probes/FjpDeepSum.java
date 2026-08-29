// RFJP.1 at a depth the shipped reproducer does not reach.
//
// `regression-suite/src/RJdkForkJoin.java` sums a long[20000] with a
// threshold of 64 — depth ~log2(20000/64) ~= 9. The RFJP.1 note says the
// miscompile appears "once the recursion depth is ~10+", so that vector sits
// just BELOW the failing depth and its pass proves nothing about the bug.
//
// This forces depth ~17 with the same shape the note describes: a long local
// (`r`) live across a RECURSIVE call, in a RecursiveTask<Long>.compute().
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveTask;

public class FjpDeepSum {
    static final class SumTask extends RecursiveTask<Long> {
        final long[] a; final int lo, hi;
        SumTask(long[] a, int lo, int hi) { this.a = a; this.lo = lo; this.hi = hi; }
        @Override protected Long compute() {
            if (hi - lo <= 2) {                 // tiny threshold => deep recursion
                long s = 0;
                for (int i = lo; i < hi; i++) s += a[i];
                return s;
            }
            int mid = (lo + hi) >>> 1;
            SumTask left = new SumTask(a, lo, mid);
            SumTask right = new SumTask(a, mid, hi);
            left.fork();
            long r = right.compute();           // long local live across recursion
            return left.join() + r;
        }
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("n", 200000);
        long[] data = new long[n];
        long expected = 0;
        for (int i = 0; i < n; i++) { data[i] = i; expected += i; }
        ForkJoinPool pool = new ForkJoinPool();
        boolean ok = true;
        for (int round = 0; round < 12; round++) {   // warm past the JIT threshold
            long got = pool.invoke(new SumTask(data, 0, data.length));
            if (got != expected) {
                System.out.println("@@FJP FAIL round=" + round + " got=" + got + " expected=" + expected);
                ok = false; break;
            }
        }
        int depth = (int) Math.ceil(Math.log((double) n / 2.0) / Math.log(2.0));
        System.out.println("@@FJP " + (ok ? "PASS" : "DEFECT") + " n=" + n + " approx_depth=" + depth + " expected=" + expected);
    }
}
