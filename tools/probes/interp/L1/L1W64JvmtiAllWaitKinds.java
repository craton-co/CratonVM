// Interpreter round i2 wave 64, lane L1: the wait kind of every door to a
// Java-level wait, as the C JVMTI table's `GetThreadState` (17), and as
// `Thread.getState()` and `ThreadMXBean.getThreadInfo` see the same thread,
// against HotSpot. Stage 3 of
// docs/internal/fixed-bugs/interpreter-i58-L1-proposal-one-published-thread-state-word-DONE-20261104.md
// (docs/internal/fixed-bugs/interpreter-i63-L1-proposal-all-wait-kinds-and-suspended-in-the-word-DONE-20261104.md;
// switch `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`).
//
// HotSpot reads the kind bit from the waiting thread's own state object
// (`JavaThreadParkedState`, `JavaThreadInObjectWaitState`,
// `JavaThreadSleepState`): a park is PARKED, `Object.wait` IN_OBJECT_WAIT,
// and `Thread.join` -- a `wait` loop on the target in JDK 25 -- IN_OBJECT_WAIT
// too, timed or not, whatever door entered it. CratonVM before wave 64 read
// IN_OBJECT_WAIT from the JMX waiting monitor (published only by
// `Object.wait`), so the served `Thread.join` (`lang_system::join_until_dead`
// and `native_thread_join_timed`, registered natives under `--compatible`)
// read PARKED: `join(ms)` 0x2a1, `join()` 0x291 (code reading; the
// orchestrator measures the before/after rows). Under `--jdk-only` the JDK's
// own `join` bytecode runs `Object.wait`, so those rows already read
// IN_OBJECT_WAIT. The `getState` and `mx` columns are the pure-Java guard:
// the switch must not move them.
//
// Each row starts a thread, waits until `Thread.getState()` answers the
// row's state, asks at once, then interrupts the thread (every wait here
// ends on an interrupt) and joins it. Two rounds.
//
// Needs the native shim tools/probes/interp/L1/L1W64JvmtiAllWaitKinds.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w64kinds.so tools/probes/interp/L1/L1W64JvmtiAllWaitKinds.c
//   javac -d /tmp/l1w64kinds tools/probes/interp/L1/L1W64JvmtiAllWaitKinds.java
//   java     -agentpath:/tmp/libl1w64kinds.so -cp /tmp/l1w64kinds L1W64JvmtiAllWaitKinds /tmp/libl1w64kinds.so
//   CRATONVM_PUBLISHED_ALL_WAIT_KINDS=1 cratonvm --compatible \
//            -agentpath:/tmp/libl1w64kinds.so -cp /tmp/l1w64kinds L1W64JvmtiAllWaitKinds /tmp/libl1w64kinds.so
//   (and again with --jdk-only, and with the switch off for the before rows)
//
// Without the argument it prints only the usage line.
//
// (ALIVE 0x1, WAITING_INDEFINITELY 0x10, WAITING_WITH_TIMEOUT 0x20,
// SLEEPING 0x40, WAITING 0x80, IN_OBJECT_WAIT 0x100, PARKED 0x200.)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// round 1
// park: err=0 state=0x291 getState=WAITING mx=WAITING
// parkNanos: err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// Condition.await: err=0 state=0x291 getState=WAITING mx=WAITING
// CountDownLatch.await(ms): err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// wait(): err=0 state=0x191 getState=WAITING mx=WAITING
// wait(ms): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(): err=0 state=0x191 getState=WAITING mx=WAITING
// join(ms): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(ms,ns): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(Duration): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// Method.invoke(join): err=0 state=0x191 getState=WAITING mx=WAITING
// sleep(ms): err=0 state=0xe1 getState=TIMED_WAITING mx=TIMED_WAITING
// parkNanos after join(ms): err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// round 2
// park: err=0 state=0x291 getState=WAITING mx=WAITING
// parkNanos: err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// Condition.await: err=0 state=0x291 getState=WAITING mx=WAITING
// CountDownLatch.await(ms): err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// wait(): err=0 state=0x191 getState=WAITING mx=WAITING
// wait(ms): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(): err=0 state=0x191 getState=WAITING mx=WAITING
// join(ms): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(ms,ns): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// join(Duration): err=0 state=0x1a1 getState=TIMED_WAITING mx=TIMED_WAITING
// Method.invoke(join): err=0 state=0x191 getState=WAITING mx=WAITING
// sleep(ms): err=0 state=0xe1 getState=TIMED_WAITING mx=TIMED_WAITING
// parkNanos after join(ms): err=0 state=0x2a1 getState=TIMED_WAITING mx=TIMED_WAITING
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.lang.reflect.Method;
import java.time.Duration;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class L1W64JvmtiAllWaitKinds {
    static native String state(String label, Thread t);

    static final long LONG_MS = 600_000L;
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Waiter {
        void await() throws Throwable;
    }

    static void await(Thread t, Thread.State state) throws InterruptedException {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != state) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for " + state + " (is " + t.getState() + ")");
                return;
            }
            Thread.sleep(1);
        }
    }

    /** A join target: alive until `release` is called. */
    static final class Target {
        volatile boolean done;
        final Thread thread = new Thread(() -> {
            while (!done) {
                LockSupport.park();
            }
        }, "l1w64-target");

        Target() {
            thread.setDaemon(true);
            thread.start();
        }

        void release() throws InterruptedException {
            done = true;
            LockSupport.unpark(thread);
            thread.join();
        }
    }

    static void row(long n, String label, Thread.State expected, Waiter waiter)
            throws InterruptedException {
        Thread t = new Thread(() -> {
            try {
                waiter.await();
            } catch (Throwable e) {
                // released by the interrupt
            }
        }, "l1w64-" + label + "-" + n);
        t.start();
        if (label.endsWith("after join(ms)")) {
            while (!joined) {
                Thread.sleep(1);
            }
        }
        await(t, expected);
        String jvmti = state(label, t);
        Thread.State getState = t.getState();
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        Thread.State mx = info == null ? null : info.getThreadState();
        System.out.println(jvmti + " getState=" + getState + " mx=" + mx);
        t.interrupt();
        t.join();
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        row(n, "park", Thread.State.WAITING, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.park();
            }
        });
        row(n, "parkNanos", Thread.State.TIMED_WAITING, () -> {
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.parkNanos(LONG_MS * 1_000_000L);
            }
        });
        ReentrantLock lock = new ReentrantLock();
        Condition condition = lock.newCondition();
        row(n, "Condition.await", Thread.State.WAITING, () -> {
            lock.lock();
            try {
                condition.await();
            } finally {
                lock.unlock();
            }
        });
        CountDownLatch latch = new CountDownLatch(1);
        row(n, "CountDownLatch.await(ms)", Thread.State.TIMED_WAITING,
                () -> latch.await(LONG_MS, TimeUnit.MILLISECONDS));
        Object monitor = new Object();
        row(n, "wait()", Thread.State.WAITING, () -> {
            synchronized (monitor) {
                monitor.wait();
            }
        });
        row(n, "wait(ms)", Thread.State.TIMED_WAITING, () -> {
            synchronized (monitor) {
                monitor.wait(LONG_MS);
            }
        });
        Target target = new Target();
        row(n, "join()", Thread.State.WAITING, () -> target.thread.join());
        row(n, "join(ms)", Thread.State.TIMED_WAITING, () -> target.thread.join(LONG_MS));
        row(n, "join(ms,ns)", Thread.State.TIMED_WAITING, () -> target.thread.join(LONG_MS, 1));
        row(n, "join(Duration)", Thread.State.TIMED_WAITING,
                () -> target.thread.join(Duration.ofMillis(LONG_MS)));
        Method reflective = Thread.class.getMethod("join");
        row(n, "Method.invoke(join)", Thread.State.WAITING, () -> reflective.invoke(target.thread));
        row(n, "sleep(ms)", Thread.State.TIMED_WAITING, () -> Thread.sleep(LONG_MS));
        // A park right after a timed join ended: the join's kind does not leak.
        joined = false;
        row(n, "parkNanos after join(ms)", Thread.State.TIMED_WAITING, () -> {
            target.thread.join(1);
            joined = true;
            while (!Thread.currentThread().isInterrupted()) {
                LockSupport.parkNanos(LONG_MS * 1_000_000L);
            }
        });
        target.release();
    }

    /** Set by the last row's waiter once its `join(1)` has returned. */
    static volatile boolean joined;

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W64JvmtiAllWaitKinds <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
