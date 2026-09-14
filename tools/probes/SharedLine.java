// Can false sharing on `GcBarrier::stw_requested` be observed at all?
//
// The flag is READ once per bytecode by every interpreting thread. Its former
// cache-line neighbours are written by `enter_blocked`/`leave_blocked` (every
// blocking native op), by `gc_generation` (every GC) and by the barrier mutex.
// So the shape that could hurt is: compute threads spinning in the interpreter
// while OTHER threads block and unblock as fast as they can.
//
//   SharedLine <computeThreads> <churnThreads> <millis>
//
// Reports total compute iterations. churn=0 is the control arm: same compute,
// nothing writing the line, so it cannot be affected by the layout and must
// not move between binaries.
public class SharedLine {
    static volatile boolean stop = false;
    static final Object LOCK = new Object();
    static long[] counts;
    static int turn = 0;
    static final java.util.concurrent.atomic.AtomicLong churn = new java.util.concurrent.atomic.AtomicLong();

    public static void main(String[] a) throws Exception {
        int nc = Integer.parseInt(a[0]), nch = Integer.parseInt(a[1]);
        long ms = Long.parseLong(a[2]);
        counts = new long[nc];
        Thread[] ts = new Thread[nc + nch];
        for (int t = 0; t < nc; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long n = 0; int s = 0;
                while (!stop) { for (int i = 0; i < 1000; i++) { s += i * 3 + 1; } n++; }
                counts[id] = n; if (s == 42) System.out.print("");
            });
        }
        // Churn: an UNTIMED wait/notify ping-pong between paired threads. Every
        // wait() goes through the VM's blocked-region bookkeeping, which is
        // what writes the flag's former neighbours. Untimed deliberately: a
        // timed wait on Windows is tick-quantized to ~15 ms, which would cap
        // the churn rate at ~66/s per thread and make the probe vacuous.
        for (int t = 0; t < nch; t++) {
            final int me = t;
            ts[nc + t] = new Thread(() -> {
                long n = 0;
                while (!stop) {
                    synchronized (LOCK) {
                        turn = (turn + 1) & 1;
                        LOCK.notifyAll();
                        if (!stop && turn != me % 2) {
                            try { LOCK.wait(); } catch (InterruptedException e) { return; }
                        }
                    }
                    n++;
                }
                churn.addAndGet(n);
            });
        }
        for (Thread t : ts) { t.setDaemon(true); t.start(); }
        Thread.sleep(ms);
        stop = true;
        synchronized (LOCK) { LOCK.notifyAll(); }
        for (int t = 0; t < nc; t++) ts[t].join(5000);
        long tot = 0; for (long c : counts) tot += c;
        System.out.println("compute_iterations=" + tot + " churn_handoffs=" + churn.get());
    }
}
