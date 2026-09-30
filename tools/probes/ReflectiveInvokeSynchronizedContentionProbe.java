// Standalone repro closer to the exact TestWebSocketFrameClient shape than
// InstanceSynchronizedContentionProbe (plain synchronized, which passed on
// both fix15 and fix16): PojoMessageHandlerWholeBase.onMessage dispatches to
// the annotated @OnMessage POJO method via java.lang.reflect.Method.invoke,
// and that target method (in the real code, indirectly, via the shared
// WsRemoteEndpointImplBase$StateMachine) enters a synchronized instance
// method. This probe combines both: many threads call Method.invoke() on a
// shared target whose invoked method is itself `synchronized`, to test
// whether the interpreter round i1 "by-name"/"native context invoke" changes
// broke this specific combination under concurrency.
import java.lang.reflect.Method;

public class ReflectiveInvokeSynchronizedContentionProbe {
    static final int THREADS = 16;
    static final int ITERS = 5000;

    static class Target {
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
            System.out.println("WATCHDOG: still not done after 30s -- HANG under reflective-invoke + synchronized contention");
            System.exit(1);
        });
        watchdog.setDaemon(true);
        watchdog.start();

        Target target = new Target();
        Method bump = Target.class.getMethod("bump");
        Thread[] threads = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            threads[t] = new Thread(() -> {
                for (int i = 0; i < ITERS; i++) {
                    try {
                        bump.invoke(target);
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                }
            });
        }
        long start = System.nanoTime();
        for (Thread th : threads) th.start();
        for (Thread th : threads) th.join();
        long elapsedMs = (System.nanoTime() - start) / 1_000_000;

        long expected = (long) THREADS * ITERS;
        long actual = target.get();
        System.out.println("expected=" + expected + " actual=" + actual + " elapsedMs=" + elapsedMs);
        if (actual != expected) {
            System.out.println("MISMATCH: lost updates under reflective-invoke + synchronized contention");
            System.exit(1);
        }
        System.out.println("ALL OK");
    }
}
