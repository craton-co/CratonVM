import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * netty's `DefaultPromise` wait/notify handshake, reduced to the three fields
 * that carry it, so the `ParameterizedSslHandlerTest` stall can be hunted
 * without netty, TLS, a selector or a 40-run rate.
 *
 * The observed stall state is `result != null` AND `waiters == 1` with the
 * waiter parked in `Object.wait()` forever. Exactly one of these must have
 * happened, and all three are unreachable with correct `synchronized` +
 * volatile semantics:
 *
 *   1. the waiter's volatile read of `result` saw `null` after the completer
 *      had already published a non-null one AND run `checkNotifyWaiters()`
 *      (legitimately seeing `waiters == 0`);
 *   2. the completer's plain read of `waiters` saw `0` after the waiter's
 *      `waiters++` inside the monitor;
 *   3. `RESULT.compareAndSet(this, null, v)` WROTE the field and reported
 *      false, so the branch containing `checkNotifyWaiters()` was never taken.
 *
 * Each leaves a distinguishable fingerprint in the STALL line:
 *
 *   cas=false result=set                -> (3)
 *   cas=true  waiters_seen=0            -> (1) or (2); `waiters_now` says which
 *   cas=true  waiters_seen&gt;0 waiters_now&gt;0 -> the notify was delivered and lost
 *
 * The waiters are a REUSED POOL spinning on a sequence number, not fresh
 * threads per round: thread creation is ~1 ms and the window being hunted is
 * the handful of instructions between `isDone()` and `wait()`, so a
 * thread-per-round harness spends all its time outside the race it is for.
 *
 * Usage: PromiseWaitProbe [rounds] [waiters] [timeoutMs] [maxSpin] [pressure]
 *
 * `pressure` adds the two ingredients the plain race does not have, because the
 * plain race alone does NOT reproduce (300 000 waits, zero stalls, on both
 * VMs):
 *
 *   `contend`  a third thread taking and releasing `synchronized (p)` in a
 *              loop, so the promise's monitor is being inflated from OUTSIDE
 *              while the waiter is entering `wait()` -- the thin/inflated
 *              handover the page names as the surface still to audit;
 *   `alloc`    per-round garbage, so a moving collection can land inside the
 *              window;
 *   `both`     both. Default: none.
 */
public class PromiseWaitProbe {

    static final class Promise {
        private static final AtomicReferenceFieldUpdater<Promise, Object> RESULT =
                AtomicReferenceFieldUpdater.newUpdater(Promise.class, Object.class, "result");

        private volatile Object result;
        private short waiters;

        /** What `checkNotifyWaiters` actually saw, for the post-mortem. */
        volatile int waitersSeenByCompleter = -1;
        /** What `compareAndSet` reported. */
        volatile boolean casReported;
        /** Whether `checkNotifyWaiters` reached `notifyAll()`. */
        volatile boolean notified;

        boolean isDone() {
            return result != null;
        }

        Object rawResult() {
            return result;
        }

        private void incWaiters() {
            ++waiters;
        }

        private void decWaiters() {
            --waiters;
        }

        /** netty `DefaultPromise.awaitUninterruptibly`, BCI-for-BCI. */
        void awaitUninterruptibly() {
            if (isDone()) {
                return;
            }
            synchronized (this) {
                while (!isDone()) {
                    incWaiters();
                    try {
                        wait();
                    } catch (InterruptedException e) {
                        // swallowed, exactly like netty's
                    } finally {
                        decWaiters();
                    }
                }
            }
        }

        private synchronized boolean checkNotifyWaiters() {
            waitersSeenByCompleter = waiters;
            if (waiters > 0) {
                notified = true;
                notifyAll();
            }
            return true;
        }

        /** netty `DefaultPromise.setValue0`. */
        boolean trySuccess(Object v) {
            boolean cas = RESULT.compareAndSet(this, null, v);
            casReported = cas;
            if (cas) {
                checkNotifyWaiters();
                return true;
            }
            return false;
        }

        /** Read under the monitor, so it cannot race the waiter's own writes. */
        synchronized int waitersNow() {
            return waiters;
        }
    }

    static volatile Promise current;
    static volatile int seq;
    static final AtomicInteger arrived = new AtomicInteger();
    static final AtomicInteger finished = new AtomicInteger();
    static volatile boolean running = true;
    static Object garbage;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int nWaiters = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        long timeoutMs = args.length > 2 ? Long.parseLong(args[2]) : 5000;
        int maxSpin = args.length > 3 ? Integer.parseInt(args[3]) : 64;
        String pressure = args.length > 4 ? args[4] : "none";
        boolean contend = pressure.equals("contend") || pressure.equals("both");
        boolean alloc = pressure.equals("alloc") || pressure.equals("both");

        if (contend) {
            Thread c = new Thread(() -> {
                int seen = 0;
                while (running) {
                    Promise p = current;
                    if (p != null) {
                        synchronized (p) {
                            seen++;
                        }
                    }
                }
                garbage = seen;
            }, "contender");
            c.setDaemon(true);
            c.start();
        }

        for (int i = 0; i < nWaiters; i++) {
            Thread w = new Thread(() -> {
                int seen = 0;
                while (running) {
                    while (running && seq == seen) {
                        Thread.onSpinWait();
                    }
                    if (!running) {
                        return;
                    }
                    seen = seq;
                    Promise p = current;
                    arrived.incrementAndGet();
                    if (p != null) {
                        p.awaitUninterruptibly();
                    }
                    finished.incrementAndGet();
                }
            }, "waiter-" + i);
            w.setDaemon(true);
            w.start();
        }

        int stalls = 0;
        long spinSeed = 12345;
        for (int r = 1; r <= rounds && stalls < 3; r++) {
            Promise p = new Promise();
            arrived.set(0);
            finished.set(0);
            current = p;
            seq = r;
            // Let some of the waiters get as far as `wait()` and some not; the
            // interesting window is the one where the completer lands between
            // a waiter's `isDone()` and its `wait()`.
            spinSeed = spinSeed * 6364136223846793005L + 1442695040888963407L;
            int spin = (int) ((spinSeed >>> 33) % (maxSpin + 1));
            for (int s = 0; s < spin; s++) {
                Thread.onSpinWait();
            }
            if (alloc) {
                for (int a = 0; a < 64; a++) {
                    garbage = new byte[512];
                }
            }
            p.trySuccess("v");

            long deadline = System.currentTimeMillis() + timeoutMs;
            while (finished.get() < nWaiters && System.currentTimeMillis() < deadline) {
                Thread.onSpinWait();
            }
            if (finished.get() < nWaiters) {
                stalls++;
                System.out.printf(
                        "STALL round=%d arrived=%d finished=%d/%d cas=%s result=%s notified=%s "
                                + "waiters_seen_by_completer=%d waiters_now=%d%n",
                        r, arrived.get(), finished.get(), nWaiters, p.casReported,
                        p.rawResult() != null ? "set" : "NULL", p.notified,
                        p.waitersSeenByCompleter, p.waitersNow());
                System.out.flush();
                // Unblock the pool so the run can continue and report a rate.
                synchronized (p) {
                    p.notifyAll();
                }
                long grace = System.currentTimeMillis() + 2000;
                while (finished.get() < nWaiters && System.currentTimeMillis() < grace) {
                    Thread.onSpinWait();
                }
                if (finished.get() < nWaiters) {
                    System.out.println("  ... and a manual notifyAll() did NOT release it either");
                    System.out.flush();
                    break;
                }
            }
            if ((r % 20000) == 0) {
                System.out.printf("... round %d, stalls=%d%n", r, stalls);
                System.out.flush();
            }
        }
        running = false;
        seq++;
        System.out.printf("PROMISE-WAIT rounds=%d waiters=%d pressure=%s stalls=%d%n",
                rounds, nWaiters, pressure, stalls);
        System.out.flush();
        Runtime.getRuntime().halt(stalls == 0 ? 0 : 1);
    }
}
