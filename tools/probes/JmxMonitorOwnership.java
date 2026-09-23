import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Correctness oracle for the per-thread JMX monitor bookkeeping that every
 * uncontended {@code monitorenter} / {@code monitorexit} maintains.
 *
 * <p>Four claims, each of which a fast path around that bookkeeping could break
 * silently:
 *
 * <ol>
 *   <li>a thread inside nested {@code synchronized} blocks reports BOTH monitors
 *       from {@code ThreadInfo.getLockedMonitors()};</li>
 *   <li>an {@code ACC_SYNCHRONIZED} method's monitor is reported the same way;</li>
 *   <li>leaving the blocks RETRACTS them — a publish with no matching retract
 *       makes the owned set grow to every object the thread ever locked;</li>
 *   <li>a contended acquire is visible as such: the blocked thread names its
 *       lock and the lock's owner, and its {@code getBlockedCount()} moves.</li>
 * </ol>
 *
 * Exit code 0 = every claim held. Run under both settings of
 * {@code CRATONVM_MONITOR_FASTPATH}; the output must be identical.
 */
public class JmxMonitorOwnership {
    static final Object LOCK_A = new Object();
    static final Object LOCK_B = new Object();
    static final Object LOCK_C = new Object();

    private int counter;

    synchronized void holdSynchronizedMethod(CountDownLatch entered, CountDownLatch release)
            throws Exception {
        counter++;
        entered.countDown();
        release.await(30, TimeUnit.SECONDS);
    }

    static int failures;

    static void check(boolean ok, String what) {
        System.out.println((ok ? "ok   " : "FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    static ThreadInfo infoOf(ThreadMXBean bean, long id) {
        for (ThreadInfo ti : bean.dumpAllThreads(true, false)) {
            if (ti != null && ti.getThreadId() == id) {
                return ti;
            }
        }
        return null;
    }

    static boolean owns(ThreadInfo ti, Object o) {
        if (ti == null) {
            return false;
        }
        for (MonitorInfo mi : ti.getLockedMonitors()) {
            if (mi.getIdentityHashCode() == System.identityHashCode(o)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        ThreadMXBean bean = ManagementFactory.getThreadMXBean();
        check(bean.isObjectMonitorUsageSupported(), "isObjectMonitorUsageSupported()");

        // ---- 1 & 3: nested blocks are published, and retracted on exit -----
        CountDownLatch entered = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(1);
        Thread holder = new Thread(() -> {
            synchronized (LOCK_A) {
                synchronized (LOCK_B) {
                    entered.countDown();
                    try {
                        release.await(30, TimeUnit.SECONDS);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                    }
                }
            }
            done.countDown();
        }, "holder");
        holder.start();
        entered.await(30, TimeUnit.SECONDS);
        ThreadInfo ti = infoOf(bean, holder.getId());
        check(owns(ti, LOCK_A), "nested blocks: outer monitor is in getLockedMonitors()");
        check(owns(ti, LOCK_B), "nested blocks: inner monitor is in getLockedMonitors()");
        release.countDown();
        done.await(30, TimeUnit.SECONDS);
        holder.join(30_000);

        // The thread is gone, so ask a LIVE thread the same question about the
        // same objects: nobody may still be reported as owning them.
        boolean stillOwned = false;
        for (ThreadInfo t : bean.dumpAllThreads(true, false)) {
            if (t != null && (owns(t, LOCK_A) || owns(t, LOCK_B))) {
                stillOwned = true;
            }
        }
        check(!stillOwned, "monitorexit retracts: no thread still owns LOCK_A / LOCK_B");

        // ---- 2: ACC_SYNCHRONIZED publishes the receiver -------------------
        JmxMonitorOwnership target = new JmxMonitorOwnership();
        CountDownLatch mEntered = new CountDownLatch(1);
        CountDownLatch mRelease = new CountDownLatch(1);
        Thread mHolder = new Thread(() -> {
            try {
                target.holdSynchronizedMethod(mEntered, mRelease);
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        }, "sync-method-holder");
        mHolder.start();
        mEntered.await(30, TimeUnit.SECONDS);
        check(owns(infoOf(bean, mHolder.getId()), target),
                "synchronized method: receiver is in getLockedMonitors()");
        mRelease.countDown();
        mHolder.join(30_000);

        // ---- 4: a contended acquire names its lock and its owner ----------
        CountDownLatch cHeld = new CountDownLatch(1);
        CountDownLatch cRelease = new CountDownLatch(1);
        Thread cOwner = new Thread(() -> {
            synchronized (LOCK_C) {
                cHeld.countDown();
                try {
                    cRelease.await(30, TimeUnit.SECONDS);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }, "c-owner");
        Thread cWaiter = new Thread(() -> {
            synchronized (LOCK_C) {
                LOCK_C.hashCode();
            }
        }, "c-waiter");
        cOwner.start();
        cHeld.await(30, TimeUnit.SECONDS);
        cWaiter.start();
        // Wait until the waiter is actually blocked on the monitor.
        ThreadInfo wi = null;
        for (int i = 0; i < 600 && wi == null; i++) {
            ThreadInfo t = infoOf(bean, cWaiter.getId());
            if (t != null && t.getThreadState() == Thread.State.BLOCKED) {
                wi = t;
            } else {
                Thread.sleep(10);
            }
        }
        check(wi != null, "contended acquire: waiter reaches BLOCKED");
        if (wi != null) {
            LockInfo li = wi.getLockInfo();
            check(li != null && li.getIdentityHashCode() == System.identityHashCode(LOCK_C),
                    "contended acquire: getLockInfo() names LOCK_C");
            check("c-owner".equals(wi.getLockOwnerName()),
                    "contended acquire: getLockOwnerName() is c-owner (got " + (wi == null ? "?" : wi.getLockOwnerName()) + ")");
            check(owns(infoOf(bean, cOwner.getId()), LOCK_C),
                    "contended acquire: the owner reports LOCK_C as owned");
            // Taken from the snapshot made WHILE blocked. A count read after
            // the thread has exited is read from a reaped entry and reports
            // -1, which is not evidence of anything -- the JMM counts the
            // block on the way IN precisely so a hang can be diagnosed while
            // it is still happening.
            check(wi.getBlockedCount() >= 1,
                    "contended acquire: getBlockedCount() >= 1 while blocked (got "
                            + wi.getBlockedCount() + ")");
        }
        cRelease.countDown();
        cOwner.join(30_000);
        cWaiter.join(30_000);

        System.out.println(failures == 0 ? "ALL OK" : (failures + " FAILURE(S)"));
        System.exit(failures == 0 ? 0 : 1);
    }
}
