import java.util.concurrent.locks.ReentrantLock;

/**
 * Are the AQS and empty-call figures steady state, or warm-up?
 *
 * `probes/AqsBreakdownProbe.java` reports an empty instance call at ~431 ns and
 * an uncontended `ReentrantLock.lock()`/`unlock()` at ~10.5 us, each from ONE
 * measured pass of 2M after a 200k warm-up. `probes/CallFloorProbe.java` and
 * `probes/CallShapeProbe.java` put an ordinary call at 7-8 ns. A 60x gap
 * between two supposedly-equivalent shapes is either a real property of the
 * call site or an artefact of how long each probe ran before it looked.
 *
 * So: run each rung repeatedly, printing every pass. A figure that is real
 * holds flat across passes. A figure that was warm-up collapses on pass 2.
 * Nothing here is averaged — the per-pass series IS the result.
 */
public final class CallFloorConvergenceProbe {

    private static final int PASSES = 6;
    private static final int ROUNDS = 2_000_000;

    private static long sink;

    private void empty() { }

    private static long emptyCall(CallFloorConvergenceProbe p, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            p.empty();
        }
        return System.nanoTime() - t0;
    }

    private static long lockUnlock(ReentrantLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    private static long syncBlock(Object m, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            synchronized (m) { sink++; }
        }
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        CallFloorConvergenceProbe p = new CallFloorConvergenceProbe();
        ReentrantLock lock = new ReentrantLock();
        Object mon = new Object();

        System.out.printf("%-26s", "pass");
        for (int i = 1; i <= PASSES; i++) {
            System.out.printf("%10d", i);
        }
        System.out.println();

        System.out.printf("%-26s", "empty instance call");
        for (int i = 0; i < PASSES; i++) {
            System.out.printf("%10.1f", emptyCall(p, ROUNDS) / (double) ROUNDS);
        }
        System.out.println("  ns/op");

        System.out.printf("%-26s", "ReentrantLock lock+unlock");
        for (int i = 0; i < PASSES; i++) {
            System.out.printf("%10.1f", lockUnlock(lock, ROUNDS) / (double) ROUNDS);
        }
        System.out.println("  ns/op");

        System.out.printf("%-26s", "synchronized block");
        for (int i = 0; i < PASSES; i++) {
            System.out.printf("%10.1f", syncBlock(mon, ROUNDS) / (double) ROUNDS);
        }
        System.out.println("  ns/op");

        if (sink == 42) { System.out.println("(unreachable)"); }
    }
}
