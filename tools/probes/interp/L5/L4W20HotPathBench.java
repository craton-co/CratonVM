/*
 * Interpreter round i1, wave 20, lane L4: three hot paths this wave changed.
 *
 *   new1 / new4, newArr1 / newArr4   (measure with --nojit)
 *       `gc_and_alloc::maybe_gc` asks the heap-occupancy triggers (ZGC's
 *       concurrent-mark start, `needs_gc`) only after a TLAB refill, an
 *       allocation outside the TLAB, or 16 KiB of this thread's allocation
 *       (`occupancy_poll_due`), instead of after every interpreted `new` /
 *       `newarray`. Should drop on every collector; most on G1, whose
 *       `needs_gc` is a process-global `fetch_add` per call, and in the
 *       4-thread rows. Also compare the GC count per run (`-Xlog:gc` or the
 *       `gc_entry_census`) between the builds: it may move only by the
 *       countdown granularity.
 *   lambda0_1 / lambda0_4             (measure with --nojit, then JIT on)
 *       a warm zero-capture lambda reads its singleton from the row's slot
 *       (`LambdaSingletonSlot`, memoised in the thread's indy entry and in a
 *       compiled site) instead of taking the process-global
 *       `LAMBDA_SINGLETON_CACHE` read lock: two atomic RMWs on a line every
 *       thread shares, plus a hash probe, per evaluation. Most in lambda0_4.
 *   concat                            (measure with the JIT on)
 *       the compiled string-concat bridge no longer pushes a synthetic
 *       `<jit-indy>.concat` frame nor builds a heap `Vec` per execution.
 *
 * Interleave against the previous build (median of rounds, per
 * `cratonvm-microbench-noise`); the best round per row goes to stderr in ns
 * per operation. stdout is deterministic and identical on HotSpot 25.
 */
import java.util.function.IntSupplier;

public class L4W20HotPathBench {
    static final int N = 400_000;
    static final int ROUNDS = 5;
    static final int THREADS = 4;

    static final class Node {
        int v;
        Node next;
    }

    static long newLoop(int n) {
        long sum = 0;
        Node keep = null;
        for (int i = 0; i < n; i++) {
            Node x = new Node();
            x.v = i;
            x.next = ((i & 31) == 0) ? null : keep;
            keep = x;
            sum += x.v & 7;
        }
        return sum;
    }

    static long newArrLoop(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            int[] a = new int[4];
            a[i & 3] = i;
            sum += a[0] & 7;
        }
        return sum;
    }

    static long lambdaLoop(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            IntSupplier s = () -> 3;
            sum += s.getAsInt();
        }
        return sum;
    }

    static long concatLoop(int n) {
        long sum = 0;
        String tag = "t";
        for (int i = 0; i < n; i++) {
            String s = tag + i + ":" + (i * 7L);
            sum += s.length();
        }
        return sum;
    }

    interface Body {
        long run(int n);
    }

    static long single(String row, Body body) {
        long best = Long.MAX_VALUE;
        long sum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            sum = body.run(N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.err.println(row + ": " + (best / N) + " ns/op");
        return sum;
    }

    static long parallel(String row, Body body) throws InterruptedException {
        long best = Long.MAX_VALUE;
        long total = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long[] sums = new long[THREADS];
            Thread[] ts = new Thread[THREADS];
            for (int k = 0; k < THREADS; k++) {
                final int slot = k;
                ts[k] = new Thread(() -> sums[slot] = body.run(N));
            }
            long t0 = System.nanoTime();
            for (Thread t : ts) t.start();
            for (Thread t : ts) t.join();
            best = Math.min(best, System.nanoTime() - t0);
            total = 0;
            for (long s : sums) total += s;
        }
        System.err.println(row + ": " + (best / N) + " ns/op (" + THREADS + " threads)");
        return total;
    }

    public static void main(String[] args) throws InterruptedException {
        long checksum = 0;
        long v;
        v = single("new1", L4W20HotPathBench::newLoop);
        System.out.println("new1 sum=" + v);
        checksum += v;
        v = parallel("new4", L4W20HotPathBench::newLoop);
        System.out.println("new4 sum=" + v);
        checksum += v;
        v = single("newArr1", L4W20HotPathBench::newArrLoop);
        System.out.println("newArr1 sum=" + v);
        checksum += v;
        v = parallel("newArr4", L4W20HotPathBench::newArrLoop);
        System.out.println("newArr4 sum=" + v);
        checksum += v;
        v = single("lambda0_1", L4W20HotPathBench::lambdaLoop);
        System.out.println("lambda0_1 sum=" + v);
        checksum += v;
        v = parallel("lambda0_4", L4W20HotPathBench::lambdaLoop);
        System.out.println("lambda0_4 sum=" + v);
        checksum += v;
        v = single("concat", L4W20HotPathBench::concatLoop);
        System.out.println("concat sum=" + v);
        checksum += v;
        System.out.println("checksum=" + checksum);
    }
}
