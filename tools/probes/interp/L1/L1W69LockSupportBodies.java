// Interpreter round i2 wave 69, lane L1:
//
// * `CRATONVM_COMPAT_JDK_LOCK_SUPPORT` (the `JdkLockSupport` HotSpot-alignment
//   row; docs/internal/fixed-bugs/interpreter-i68-L1-proposal-let-lock-support-blocker-overloads-run-their-bytecode-DONE-20261101.md):
//   `--compatible` over a real JDK no longer registers the seven retired
//   `LockSupport` stand-ins, so `park*(Object, ..)`, `unpark` and `getBlocker`
//   run the JDK 25 bodies (`setBlocker(t, blocker)` around `Unsafe.park`,
//   `getBlocker` reading `parkBlocker`), as `--jdk-only` already did;
// * `CRATONVM_PROCESS_ONE_WAIT_CONDITION`
//   (docs/internal/fixed-bugs/interpreter-i68-L1-proposal-one-condition-per-synthetic-process-DONE-20261101.md):
//   every `waitFor` of one VM-spawned process names ONE `ConditionObject`, as
//   UNIX `ProcessImpl`'s final `condition` does on HotSpot.
//
// Columns, read without an agent: the state; `jmx-same` (JMX agrees);
// `lock=` the class of `ThreadInfo.getLockName()`; `blocker=` the class of
// `LockSupport.getBlocker(t)`. `same-lock=` compares two threads' full
// `getLockName()` strings (class AND identity hash).
//
// Known remaining difference (not these switches): the `--compatible`
// `CountDownLatch.await` stand-in sets no `parkBlocker`
// (`i68-L1-countdownlatch-stand-in-sets-no-park-blocker-20261101.md`), so its
// `blocker=` reads null there.
//
// Run in both modes: `cratonvm L1W69LockSupportBodies` (jdk-only) and
// `cratonvm --compatible L1W69LockSupportBodies`; both must print the block
// below. `=0` arms: `CRATONVM_COMPAT_JDK_LOCK_SUPPORT=0` brings the wave-68
// stand-ins back (the `virtual` row's blocker and the `stack` row move);
// `CRATONVM_PROCESS_ONE_WAIT_CONDITION=0` prints `same-lock=false` on the two
// process rows. The process rows need a `sleep` binary (UNIX).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// LockSupport.park(blocker): WAITING jmx-same=true lock=L1W69LockSupportBodies$Marker blocker=L1W69LockSupportBodies$Marker
// LockSupport.parkNanos(blocker, 60s): TIMED_WAITING jmx-same=true lock=L1W69LockSupportBodies$Marker blocker=L1W69LockSupportBodies$Marker
// LockSupport.parkUntil(blocker, +60s): TIMED_WAITING jmx-same=true lock=L1W69LockSupportBodies$Marker blocker=L1W69LockSupportBodies$Marker
// LockSupport.park(): WAITING jmx-same=true lock=null blocker=null
// LockSupport.parkNanos(60s): TIMED_WAITING jmx-same=true lock=null blocker=null
// ReentrantLock.lock: WAITING jmx-same=true lock=java.util.concurrent.locks.ReentrantLock$NonfairSync blocker=java.util.concurrent.locks.ReentrantLock$NonfairSync
// Condition.awaitNanos: TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject blocker=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// CountDownLatch.await(): WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync blocker=java.util.concurrent.CountDownLatch$Sync
// blocker after unpark: null
// virtual LockSupport.park(blocker): WAITING blocker=L1W69LockSupportBodies$Marker
// stack(LockSupport.park(blocker)): jdk.internal.misc.Unsafe.park > java.util.concurrent.locks.LockSupport.park
// Process.waitFor() x2: WAITING WAITING same-lock=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// Process.waitFor() then waitFor(ms): TIMED_WAITING same-lock=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// getBlocker(running thread): null
// getBlocker(null): java.lang.NullPointerException
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class L1W69LockSupportBodies {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    static final class Marker {
    }

    interface Blocking {
        void run() throws Exception;
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String className(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String lockClass(String name) {
        if (name == null) {
            return "null";
        }
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    static Thread start(String label, Blocking body) {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (InterruptedException e) {
                // Interrupted at the end of the row.
            } catch (Exception e) {
                System.out.println(label + ": threw " + e);
            }
        }, label);
        t.setDaemon(true);
        t.start();
        return t;
    }

    static void settle(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
    }

    /** A reading in which the thread waits and JMX agrees (and names a lock if it ever will). */
    static ThreadInfo reading(Thread t, boolean wantLock) throws InterruptedException {
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            Thread.State s = t.getState();
            if (waiting(s) && info != null && info.getThreadState() == s
                    && (!wantLock || info.getLockName() != null)) {
                break;
            }
            Thread.sleep(2);
            info = MX.getThreadInfo(t.threadId());
        }
        return info;
    }

    static void row(String label, boolean wantLock, Blocking body) throws Exception {
        Thread t = start(label, body);
        settle(t);
        ThreadInfo info = reading(t, wantLock);
        Thread.State s = t.getState();
        Object blocker = LockSupport.getBlocker(t);
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same
                + " lock=" + lockClass(info == null ? null : info.getLockName())
                + " blocker=" + className(blocker));
        t.interrupt();
        t.join(2000);
    }

    public static void main(String[] args) throws Exception {
        Marker marker = new Marker();
        row("LockSupport.park(blocker)", true, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.park(marker);
            }
        });
        row("LockSupport.parkNanos(blocker, 60s)", true, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.parkNanos(marker, TimeUnit.SECONDS.toNanos(60));
            }
        });
        row("LockSupport.parkUntil(blocker, +60s)", true, () -> {
            long until = System.currentTimeMillis() + 60_000;
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.parkUntil(marker, until);
            }
        });
        row("LockSupport.park()", false, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.park();
            }
        });
        row("LockSupport.parkNanos(60s)", false, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.parkNanos(TimeUnit.SECONDS.toNanos(60));
            }
        });

        ReentrantLock held = new ReentrantLock();
        held.lock();
        row("ReentrantLock.lock", true, () -> held.lockInterruptibly());
        held.unlock();

        ReentrantLock lock = new ReentrantLock();
        Condition cond = lock.newCondition();
        row("Condition.awaitNanos", true, () -> {
            lock.lock();
            try {
                cond.awaitNanos(TimeUnit.SECONDS.toNanos(60));
            } finally {
                lock.unlock();
            }
        });

        row("CountDownLatch.await()", true, () -> new CountDownLatch(1).await());

        // The JDK body's `finally { setBlocker(t, null); }`: once the park
        // returns, the thread's own blocker reads null again.
        Object[] after = new Object[1];
        Thread parker = start("parker", () -> {
            LockSupport.park(marker);
            after[0] = LockSupport.getBlocker(Thread.currentThread());
        });
        settle(parker);
        LockSupport.unpark(parker);
        parker.join(5000);
        System.out.println("blocker after unpark: " + className(after[0]));

        // A virtual thread's park sets the blocker on the virtual thread too
        // (`setBlocker(t, ..)` precedes `JLA.parkVirtualThread()`).
        Thread vt = Thread.ofVirtual().start(() -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.park(marker);
            }
        });
        settle(vt);
        System.out.println("virtual LockSupport.park(blocker): " + vt.getState()
                + " blocker=" + className(LockSupport.getBlocker(vt)));
        vt.interrupt();
        vt.join(2000);

        // The two top frames of a thread parked in `LockSupport.park(blocker)`.
        Thread stacked = start("stacked", () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.park(marker);
            }
        });
        settle(stacked);
        StackTraceElement[] st = stacked.getStackTrace();
        StringBuilder top = new StringBuilder();
        for (int i = 0; i < Math.min(2, st.length); i++) {
            if (i > 0) {
                top.append(" > ");
            }
            top.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        System.out.println("stack(LockSupport.park(blocker)): " + top);
        stacked.interrupt();
        stacked.join(2000);

        // Two waiters of one process name one condition (class and hash).
        Process p = new ProcessBuilder("sleep", "30").start();
        Thread w1 = start("waitFor-1", () -> p.waitFor());
        Thread w2 = start("waitFor-2", () -> p.waitFor());
        settle(w1);
        settle(w2);
        ThreadInfo i1 = reading(w1, true);
        ThreadInfo i2 = reading(w2, true);
        String n1 = i1 == null ? null : i1.getLockName();
        String n2 = i2 == null ? null : i2.getLockName();
        System.out.println("Process.waitFor() x2: " + w1.getState() + " " + w2.getState()
                + " same-lock=" + (n1 != null && n1.equals(n2)) + " lock=" + lockClass(n1));
        // A later timed waiter of the same process names the same one.
        Thread w3 = start("waitFor-3", () -> p.waitFor(60, TimeUnit.SECONDS));
        settle(w3);
        ThreadInfo i3 = reading(w3, true);
        String n3 = i3 == null ? null : i3.getLockName();
        System.out.println("Process.waitFor() then waitFor(ms): " + w3.getState()
                + " same-lock=" + (n1 != null && n1.equals(n3)) + " lock=" + lockClass(n3));
        w1.interrupt();
        w2.interrupt();
        w3.interrupt();
        w1.join(2000);
        w2.join(2000);
        w3.join(2000);
        p.destroyForcibly();
        p.waitFor(5, TimeUnit.SECONDS);

        System.out.println("getBlocker(running thread): " + className(LockSupport.getBlocker(Thread.currentThread())));
        try {
            LockSupport.getBlocker(null);
            System.out.println("getBlocker(null): no exception");
        } catch (RuntimeException e) {
            System.out.println("getBlocker(null): " + e.getClass().getName());
        }
        System.out.println("done");
    }
}
