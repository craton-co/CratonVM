// Standalone repro probing whether many threads hammering the SAME instance's
// synchronized methods can hang under CratonVM, modeled on the exact shape
// suspected in the TestWebSocketFrameClient post-dev-merge regression:
// WsRemoteEndpointImplBase$StateMachine is a per-connection instance with
// several `synchronized void xStart()`/`complete()` methods, called from many
// concurrent writer threads. This probe is NOT the state-machine's own logic
// (no illegal-state checks) -- it isolates just the monitor-contention shape:
// many threads repeatedly entering/exiting synchronized instance methods on
// one shared object, racing against occasional exceptions and re-entry.
public class InstanceSynchronizedContentionProbe {
    static final int THREADS = 16;
    static final int ITERS = 20000;

    static class Counter {
        private long value = 0;

        public synchronized void bump() {
            value++;
        }

        public synchronized long get() {
            return value;
        }
    }

    public static void main(String[] args) throws Exception {
        Thread watchdog = new Thread(() -> {
            try { Thread.sleep(30000); } catch (InterruptedException ignored) { return; }
            System.out.println("WATCHDOG: still not done after 30s -- HANG under instance-synchronized contention");
            System.exit(1);
        });
        watchdog.setDaemon(true);
        watchdog.start();

        Counter counter = new Counter();
        Thread[] threads = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            threads[t] = new Thread(() -> {
                for (int i = 0; i < ITERS; i++) {
                    counter.bump();
                }
            });
        }
        long start = System.nanoTime();
        for (Thread th : threads) th.start();
        for (Thread th : threads) th.join();
        long elapsedMs = (System.nanoTime() - start) / 1_000_000;

        long expected = (long) THREADS * ITERS;
        long actual = counter.get();
        System.out.println("expected=" + expected + " actual=" + actual + " elapsedMs=" + elapsedMs);
        if (actual != expected) {
            System.out.println("MISMATCH: lost updates under instance-synchronized contention");
            System.exit(1);
        }
        System.out.println("ALL OK");
    }
}
