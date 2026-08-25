/**
 * Positive control for the residual-stall instruments.
 *
 * Two threads park in `Object.wait()` on two DIFFERENT objects and nobody ever
 * notifies either. The watchdog must therefore print a `[WAIT-CENSUS]` with
 * exactly TWO rows, naming both objects — which is the property residual 1
 * needs and the single-thread dump could never show. One of the two objects
 * carries a `result` field so the slot-provenance line has something to
 * resolve; the other has none, so the `result_slot=UNRESOLVED` arm is exercised
 * too.
 *
 * Run it BEFORE trusting a quiet census on a real stall: a census that prints
 * nothing because the instrument is absent looks exactly like a census that
 * prints nothing because no thread is waiting.
 */
public final class WaitCensusProbe {
    /** Same field name and declared type as `io.netty.util.concurrent.DefaultPromise.result`. */
    static final class FakePromise {
        private volatile Object result;
        private short waiters;
        @SuppressWarnings("unused")
        void touch() { waiters++; }
    }

    static final class Plain { }

    private static void parkForever(final Object lock, String who) {
        Thread t = new Thread(() -> {
            synchronized (lock) {
                while (true) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, who);
        t.setDaemon(true);
        t.start();
    }

    public static void main(String[] args) throws Exception {
        FakePromise withResult = new FakePromise();
        Plain withoutResult = new Plain();
        parkForever(withResult, "waiter-with-result");
        parkForever(withoutResult, "waiter-without-result");
        System.out.println("PROBE: two waiters parked; waiting for the watchdog");
        System.out.flush();
        Thread.sleep(600_000L);
    }
}
