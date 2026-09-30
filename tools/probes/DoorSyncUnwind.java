import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Every way an {@code ACC_SYNCHRONIZED} frame can leave, driven hard enough to
 * reach the interpreter's fast doors.
 *
 * <p>A door that acquires the callee's monitor and forgets to release it does
 * not fail here loudly — it deadlocks the next thread that wants the same lock,
 * or it silently leaves the monitor in {@code getLockedMonitors()} forever. So
 * every arm below ends by proving the lock is FREE again, from a second thread
 * that must be able to take it, and from JMX.
 *
 * <ol>
 *   <li>normal return, millions of times (the door's steady state);</li>
 *   <li>return by exception through a synchronized frame;</li>
 *   <li>an exception caught SEVERAL synchronized frames up, so the unwind
 *       pops more than one monitor-owning frame at once;</li>
 *   <li>recursion, so one thread owns the same monitor many times over;</li>
 *   <li>real contention between threads, which is the arm a door must decline
 *       and hand to the general path;</li>
 *   <li>a synchronized method that itself blocks on a second monitor.</li>
 * </ol>
 *
 * Exit 0 = every arm finished and every monitor came back free.
 */
public class DoorSyncUnwind {
    static int failures;

    static void check(boolean ok, String what) {
        System.out.println((ok ? "ok   " : "FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    // ---- the synchronized callees the doors should now serve -------------
    static final class Box {
        private int v;
        synchronized int get() { return v; }
        synchronized void inc() { v++; }
        synchronized int boom() { throw new IllegalStateException("boom"); }
        synchronized int deep(int n) { return n == 0 ? v : deep(n - 1) + 1; }
        synchronized int deepBoom(int n) {
            if (n == 0) {
                throw new IllegalStateException("deep boom");
            }
            return deepBoom(n - 1);
        }
        synchronized int lockAlso(Object other) {
            synchronized (other) {
                return v;
            }
        }
    }

    /** Can a SECOND thread take this monitor? The only real proof it is free. */
    static boolean lockIsFree(Object o) throws Exception {
        final boolean[] got = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (o) {
                got[0] = true;
            }
        }, "prober");
        t.start();
        t.join(10_000);
        return got[0] && !t.isAlive();
    }

    static boolean nobodyOwns(Object o) {
        ThreadMXBean bean = ManagementFactory.getThreadMXBean();
        if (!bean.isObjectMonitorUsageSupported()) {
            return true;
        }
        for (ThreadInfo ti : bean.dumpAllThreads(true, false)) {
            if (ti == null) {
                continue;
            }
            for (MonitorInfo mi : ti.getLockedMonitors()) {
                if (mi.getIdentityHashCode() == System.identityHashCode(o)) {
                    return false;
                }
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        Box b = new Box();

        // 1. Normal return, hot enough that every door is warm.
        for (int i = 0; i < n; i++) {
            b.inc();
            if (b.get() < 0) {
                throw new AssertionError();
            }
        }
        check(b.get() == n, "normal return: " + n + " synchronized calls, value == " + n);
        check(lockIsFree(b), "normal return: monitor is free afterwards");
        check(nobodyOwns(b), "normal return: no thread reports owning it");

        // 2. Return by exception, repeatedly.
        int caught = 0;
        for (int i = 0; i < n / 10; i++) {
            try {
                b.boom();
            } catch (IllegalStateException e) {
                caught++;
            }
        }
        check(caught == n / 10, "throwing return: caught " + caught);
        check(lockIsFree(b), "throwing return: monitor is free afterwards");
        check(nobodyOwns(b), "throwing return: no thread reports owning it");

        // 3. Unwind through MANY synchronized frames at once.
        caught = 0;
        for (int i = 0; i < n / 100; i++) {
            try {
                b.deepBoom(30);
            } catch (IllegalStateException e) {
                caught++;
            }
        }
        check(caught == n / 100, "deep unwind: caught " + caught + " through 30 frames each");
        check(lockIsFree(b), "deep unwind: monitor is free afterwards");
        check(nobodyOwns(b), "deep unwind: no thread reports owning it");

        // 4. Re-entrant recursion: one thread, many acquisitions of one monitor.
        int deep = 0;
        for (int i = 0; i < n / 100; i++) {
            deep = b.deep(40);
        }
        check(deep == n + 40, "recursion: 40 nested acquisitions returned " + deep);
        check(lockIsFree(b), "recursion: monitor is free afterwards");

        // 5. Real contention: the arm a door must DECLINE.
        final Box shared = new Box();
        final int threads = 4;
        final int each = Math.max(1, n / 20);
        Thread[] ts = new Thread[threads];
        final CountDownLatch go = new CountDownLatch(1);
        final AtomicInteger done = new AtomicInteger();
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                try {
                    go.await(30, TimeUnit.SECONDS);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                for (int i = 0; i < each; i++) {
                    shared.inc();
                }
                done.incrementAndGet();
            }, "contend-" + t);
            ts[t].start();
        }
        go.countDown();
        for (Thread t : ts) {
            t.join(120_000);
        }
        check(done.get() == threads, "contention: all " + threads + " threads finished");
        check(shared.get() == threads * each,
                "contention: count is exact (" + shared.get() + " of " + (threads * each) + ")");
        check(lockIsFree(shared), "contention: monitor is free afterwards");

        // 6. A synchronized method that blocks on a second monitor.
        final Object other = new Object();
        int v = 0;
        for (int i = 0; i < n / 100; i++) {
            v = b.lockAlso(other);
        }
        check(v == n, "nested monitors: returned " + v);
        check(lockIsFree(b) && lockIsFree(other), "nested monitors: both are free afterwards");

        System.out.println(failures == 0 ? "ALL OK" : (failures + " FAILURE(S)"));
        System.exit(failures == 0 ? 0 : 1);
    }
}
