/*
 * MonitorCostProbe - price ONE uncontended monitorenter/monitorexit pair.
 *
 * SyncMethodProbe shows that `synchronized` code runs 50-150x slower than the
 * same code without the monitor, but its bodies are big enough that "the whole
 * method stayed interpreted" and "the monitor itself is expensive" are not
 * separable. These bodies are as close to empty as Java allows, so the
 * difference between them IS the monitor pair (plus two bytecodes).
 *
 * Neither variant is JIT-compiled on this VM today (ACC_SYNCHRONIZED is an
 * unconditional skip; a `synchronized` block is refused by RBC.6), so this
 * measures the INTERPRETER's monitor path — which is what a webapp deploy
 * pays, since deploy code never gets hot enough to compile anyway.
 *
 * `lockUnlock` is the same question for `java.util.concurrent.locks`, the
 * other form JDK 25's `BufferedInputStream.read()` can take.
 *
 * Usage:  cratonvm -cp <dir> MonitorCostProbe [iters]
 */
import java.util.concurrent.locks.ReentrantLock;

public class MonitorCostProbe {

    private int field = 7;
    private final Object lock = new Object();
    private final ReentrantLock rl = new ReentrantLock();

    int plain()            { return field; }
    int syncThis()         { synchronized (this) { return field; } }
    int syncOther()        { synchronized (lock) { return field; } }
    synchronized int syncMethod() { return field; }
    int lockUnlock()       { rl.lock(); try { return field; } finally { rl.unlock(); } }

    static int sink;

    interface Body { int run(MonitorCostProbe p); }

    static double time(MonitorCostProbe p, int iters, int which) {
        int s = 0;
        long t0 = System.nanoTime();
        switch (which) {
            case 0: for (int i = 0; i < iters; i++) { s += p.plain(); }      break;
            case 1: for (int i = 0; i < iters; i++) { s += p.syncThis(); }   break;
            case 2: for (int i = 0; i < iters; i++) { s += p.syncOther(); }  break;
            case 3: for (int i = 0; i < iters; i++) { s += p.syncMethod(); } break;
            default: for (int i = 0; i < iters; i++) { s += p.lockUnlock(); } break;
        }
        long dt = System.nanoTime() - t0;
        sink += s;
        return (double) dt / iters;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        MonitorCostProbe p = new MonitorCostProbe();
        for (int round = 1; round <= 3; round++) {
            double a = time(p, iters, 0);
            double b = time(p, iters, 1);
            double c = time(p, iters, 2);
            double d = time(p, iters, 3);
            double e = time(p, iters, 4);
            System.out.println(String.format(
                "round %d  plain=%.0f syncThis=%.0f syncOther=%.0f syncMethod=%.0f lockUnlock=%.0f ns/op"
                + "   |  monitor pair ~= %.0f ns",
                round, a, b, c, d, e, b - a));
        }
        System.out.println("sink=" + sink);
    }
}
