// Does VarHandle throughput SCALE with threads?
//
// `varhandle_instance_field_plan` and `vh_meta_get` both take
// `VH_META_TABLE`, a single process-global parking_lot::Mutex around one map,
// on EVERY VarHandle operation -- the read fast path, the write fast path and
// the generic funnel alike. If that serializes, the per-op nanoseconds are the
// smaller half of the problem: the whole java.util.concurrent layer built on
// VarHandle stops scaling.
//
// Each thread here owns its OWN object and its own field, so there is no
// contention on the DATA. Any flattening is contention on the VM's side.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.CountDownLatch;

public class HibfixVarHandleScale {

    static class Cell { volatile Object ref; volatile int num; }

    static final VarHandle REF;
    static final VarHandle NUM;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(Cell.class, "ref", Object.class);
            NUM = l.findVarHandle(Cell.class, "num", int.class);
        } catch (Exception e) { throw new ExceptionInInitializerError(e); }
    }

    static double run(int threads, int itersPerThread) throws Exception {
        Cell[] cells = new Cell[threads];
        for (int i = 0; i < threads; i++) cells[i] = new Cell();
        CountDownLatch ready = new CountDownLatch(threads);
        CountDownLatch go = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(threads);
        for (int t = 0; t < threads; t++) {
            final Cell c = cells[t];
            final Object a = new Object(), b = new Object();
            new Thread(() -> {
                ready.countDown();
                try { go.await(); } catch (InterruptedException e) { return; }
                for (int i = 0; i < itersPerThread; i++) {
                    REF.set(c, (i & 1) == 0 ? a : b);   // the bound write path
                    NUM.set(c, i);                       // and the primitive one
                }
                done.countDown();
            }, "vh-" + t).start();
        }
        ready.await();
        long t0 = System.nanoTime();
        go.countDown();
        done.await();
        long ns = System.nanoTime() - t0;
        long ops = (long) threads * itersPerThread * 2;
        return ops * 1_000_000_000.0 / ns; // ops/sec
    }

    public static void main(String[] args) throws Exception {
        int iters = Integer.getInteger("probe.iters", 300000);
        run(2, 50000); // warm
        double base = 0;
        for (int threads : new int[] {1, 2, 4, 8, 16, 24}) {
            double ops = run(threads, iters);
            if (threads == 1) base = ops;
            System.out.printf("@@SCALE threads=%2d ops_per_sec=%,12.0f speedup_vs_1t=%5.2fx%n",
                    threads, ops, ops / base);
        }
    }
}
