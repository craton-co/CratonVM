import io.netty.util.concurrent.FastThreadLocal;
import io.netty.util.internal.InternalThreadLocalMap;

/**
 * The actual loop `FastThreadLocalTest.testConstructionWithIndex` runs, timed
 * over a bounded slice instead of its full 2 147 483 639 iterations.
 */
public class FtlRate {
    static Object sink;

    static long loop(int n) {
        long t = System.nanoTime();
        for (int i = 0; i < n; i++) { sink = new FastThreadLocal<Boolean>(); }
        return System.nanoTime() - t;
    }

    public static void main(String[] a) {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 1_000_000;
        int n    = a.length > 1 ? Integer.parseInt(a[1]) : 10_000_000;
        loop(warm);
        int before = InternalThreadLocalMap.lastVariableIndex();
        long ns = loop(n);
        int after = InternalThreadLocalMap.lastVariableIndex();
        double perSec = n / (ns / 1e9);
        System.out.printf("ftl ns/op=%.1f ops/s=%.0f advanced=%d fullLoopSec=%.0f%n",
            (double) ns / n, perSec, after - before, 2147483639.0 / perSec);
    }
}
