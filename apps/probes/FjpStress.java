// RFJP.1 acceptance, widened: every ForkJoinTask shape the blocklist covers,
// at a depth well past the "~10+" the original note names, with the boxing
// unbox-on-recursive-return the note points at kept on the hot path.
//
// Three shapes, because `is_fjp_subclass_blocklisted` names three JDK bases:
//   * RecursiveTask<Long>  — the original RFJP.1 shape (Long.valueOf boxing)
//   * RecursiveAction      — void, result written to a shared array
//   * CountedCompleter<Long> — the third base, different completion protocol
import java.util.concurrent.CountedCompleter;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.RecursiveAction;
import java.util.concurrent.RecursiveTask;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class FjpStress {

    static final AtomicInteger MAX_DEPTH = new AtomicInteger();

    static void depth(int d) {
        int cur;
        while (d > (cur = MAX_DEPTH.get())) {
            if (MAX_DEPTH.compareAndSet(cur, d)) return;
        }
    }

    /** RecursiveTask<Long>: the boxed-return shape RFJP.1 was recorded against. */
    static final class BoxedSum extends RecursiveTask<Long> {
        final long[] a; final int lo, hi, d;
        BoxedSum(long[] a, int lo, int hi, int d) { this.a = a; this.lo = lo; this.hi = hi; this.d = d; }
        @Override protected Long compute() {
            depth(d);
            if (hi - lo <= 2) {
                long s = 0;
                for (int i = lo; i < hi; i++) s += a[i];
                return s;
            }
            int mid = (lo + hi) >>> 1;
            BoxedSum left = new BoxedSum(a, lo, mid, d + 1);
            left.fork();
            BoxedSum right = new BoxedSum(a, mid, hi, d + 1);
            // Both unbox a Long returned from a recursive call, and `r` is a
            // long local live ACROSS the recursion — the exact site named.
            long r = right.compute();
            long l = left.join();
            return l + r;
        }
    }

    /** RecursiveAction: no boxing, result via a shared array. */
    static final class VoidSum extends RecursiveAction {
        final long[] a; final int lo, hi, d; final long[] out; final int slot;
        VoidSum(long[] a, int lo, int hi, int d, long[] out, int slot) {
            this.a = a; this.lo = lo; this.hi = hi; this.d = d; this.out = out; this.slot = slot;
        }
        @Override protected void compute() {
            depth(d);
            if (hi - lo <= 2) {
                long s = 0;
                for (int i = lo; i < hi; i++) s += a[i];
                out[slot] = s;
                return;
            }
            int mid = (lo + hi) >>> 1;
            long[] parts = new long[2];
            VoidSum left = new VoidSum(a, lo, mid, d + 1, parts, 0);
            left.fork();
            VoidSum right = new VoidSum(a, mid, hi, d + 1, parts, 1);
            right.compute();
            left.join();
            out[slot] = parts[0] + parts[1];
        }
    }

    /** CountedCompleter: the third base the blocklist names. */
    static final class CcSum extends CountedCompleter<Long> {
        final long[] a; final int lo, hi, d;
        long result;
        CcSum left, right;
        CcSum(CcSum parent, long[] a, int lo, int hi, int d) {
            super(parent); this.a = a; this.lo = lo; this.hi = hi; this.d = d;
        }
        @Override public void compute() {
            depth(d);
            if (hi - lo <= 2) {
                long s = 0;
                for (int i = lo; i < hi; i++) s += a[i];
                result = s;
            } else {
                int mid = (lo + hi) >>> 1;
                left = new CcSum(this, a, lo, mid, d + 1);
                right = new CcSum(this, a, mid, hi, d + 1);
                setPendingCount(2);
                left.fork();
                right.fork();
            }
            tryComplete();
        }
        @Override public void onCompletion(CountedCompleter<?> caller) {
            if (left != null) result = left.result + right.result;
        }
        @Override public Long getRawResult() { return result; }
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("n", 200000);
        int rounds = Integer.getInteger("rounds", 20);
        long[] data = new long[n];
        long expected = 0;
        for (int i = 0; i < n; i++) { data[i] = i; expected += i; }
        ForkJoinPool pool = new ForkJoinPool();
        AtomicLong bad = new AtomicLong();

        for (int r = 0; r < rounds; r++) {
            long got = pool.invoke(new BoxedSum(data, 0, n, 0));
            if (got != expected) { System.out.println("@@FJPSTRESS FAIL shape=RecursiveTask round=" + r + " got=" + got); bad.incrementAndGet(); break; }
        }
        for (int r = 0; r < rounds; r++) {
            long[] out = new long[1];
            pool.invoke(new VoidSum(data, 0, n, 0, out, 0));
            if (out[0] != expected) { System.out.println("@@FJPSTRESS FAIL shape=RecursiveAction round=" + r + " got=" + out[0]); bad.incrementAndGet(); break; }
        }
        for (int r = 0; r < rounds; r++) {
            long got = pool.invoke(new CcSum(null, data, 0, n, 0));
            if (got != expected) { System.out.println("@@FJPSTRESS FAIL shape=CountedCompleter round=" + r + " got=" + got); bad.incrementAndGet(); break; }
        }
        pool.shutdown();
        System.out.println("@@FJPSTRESS " + (bad.get() == 0 ? "PASS" : "DEFECT")
                + " n=" + n + " rounds=" + rounds + " max_depth=" + MAX_DEPTH.get()
                + " expected=" + expected);
    }
}
