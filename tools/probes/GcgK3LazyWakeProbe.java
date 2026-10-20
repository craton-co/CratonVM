// gcg g3/k: pauses landing while threads sit in the running monitor park,
// with many idle inflated monitors in the index.
//
// The workload `CRATONVM_MONITOR_LAZY_WAKE_LIST` changes: every winning pause
// request calls `MonitorTable::wake_lazy_parkers`, which (by default) walks
// every inflated monitor of all 64 index shards whenever any thread is in the
// running park. Here `<monitors>` idle objects are inflated once at start-up
// (`notify` under the lock inflates), a holder thread holds a hot lock in
// short slices so `<contenders>` threads keep entering the running park, and
// an allocator forces young collections, so pauses keep arriving with a lazy
// parker present.
//
//   GcgK3LazyWakeProbe <monitors> <contenders> <millis>
//
// stdout (deterministic, diffed against HotSpot): one K3LAZYWAKE line saying
// whether the hot-lock counter agrees with the per-thread counts and whether
// every idle monitor is still usable. stderr: wall time, acquisitions.
// Read `[stw-ttsp]` / `ttsp_us` (CRATONVM_DBG_STW_CENSUS=1) and the
// `lazy_wake_walks` / `lazy_wake_lists` counts (CRATONVM_DBG_MONITOR_CONTENTION=1).
public final class GcgK3LazyWakeProbe {
    static final Object HOT = new Object();
    static volatile boolean stop;
    static volatile Object sink;
    static long counter;

    public static void main(String[] a) throws Exception {
        final int monitors = Integer.parseInt(a[0]);
        final int contenders = Integer.parseInt(a[1]);
        final long millis = Long.parseLong(a[2]);

        final Object[] idle = new Object[monitors];
        for (int i = 0; i < monitors; i++) {
            Object o = new Object();
            synchronized (o) {
                o.notify();
            }
            idle[i] = o;
        }

        final long[] perThread = new long[contenders];
        Thread[] ts = new Thread[contenders];
        for (int c = 0; c < contenders; c++) {
            final int me = c;
            ts[c] = new Thread(() -> {
                long n = 0;
                while (!stop) {
                    synchronized (HOT) {
                        counter++;
                    }
                    n++;
                }
                perThread[me] = n;
            }, "k3-contender-" + c);
        }
        Thread holder = new Thread(() -> {
            try {
                while (!stop) {
                    synchronized (HOT) {
                        Thread.sleep(3);
                    }
                    Thread.sleep(1);
                }
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        }, "k3-holder");
        Thread alloc = new Thread(() -> {
            while (!stop) {
                sink = new byte[4096];
            }
        }, "k3-alloc");
        alloc.setDaemon(true);

        long t0 = System.nanoTime();
        holder.start();
        alloc.start();
        for (Thread t : ts) {
            t.start();
        }
        Thread.sleep(millis);
        stop = true;
        holder.join();
        for (Thread t : ts) {
            t.join();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        long sum = 0;
        for (long n : perThread) {
            sum += n;
        }
        boolean consistent;
        synchronized (HOT) {
            consistent = sum == counter;
        }
        boolean idleOk = true;
        for (Object o : idle) {
            synchronized (o) {
                o.notifyAll();
            }
            if (o == null) {
                idleOk = false;
            }
        }
        System.out.println("K3LAZYWAKE monitors=" + monitors + " contenders=" + contenders
                + " consistent=" + consistent + " idle_ok=" + idleOk);
        System.err.println("[k3-lazywake] ms=" + ms + " acquisitions=" + sum
                + " per_ms=" + (ms == 0 ? 0 : sum / ms));
    }
}
