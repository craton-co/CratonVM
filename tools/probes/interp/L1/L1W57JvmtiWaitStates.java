// Interpreter round i2 wave 57, lane L1: the C JVMTI table's `GetThreadState`
// (17) of a thread in each kind of wait, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-jvmti-thread-state-of-an-object-wait-waiter-is-runnable-under-jdk-only-FIXED-20261021.md).
//
// The native `state` prints the whole `jvmtiThreadState` word of one thread.
// Each row starts a thread, waits until `Thread.getState()` answers the
// state the wait should give, and asks at once (the `Object.wait` row first:
// it read RUNNABLE under `--jdk-only` while `getState()` already said
// WAITING); then it releases the thread and joins it. Two rounds (the
// first-call rows again).
//
// Needs the native shim tools/probes/interp/L1/L1W57JvmtiWaitStates.c, loaded
// both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w57states.so tools/probes/interp/L1/L1W57JvmtiWaitStates.c
//   javac -d /tmp/l1w57states tools/probes/interp/L1/L1W57JvmtiWaitStates.java
//   java     -agentpath:/tmp/libl1w57states.so -cp /tmp/l1w57states L1W57JvmtiWaitStates /tmp/libl1w57states.so
//   cratonvm -agentpath:/tmp/libl1w57states.so -cp /tmp/l1w57states L1W57JvmtiWaitStates /tmp/libl1w57states.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W57JvmtiWaitStates (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   wait(): err=0 state=0x191
//   wait(ms): err=0 state=0x1a1
//   park: err=0 state=0x291
//   parkNanos: err=0 state=0x2a1
//   sleep: err=0 state=0xe1
//   join: err=0 state=0x191
//   blocked: err=0 state=0x401
//   running: err=0 state=0x5
//   wait(): err=0 state=0x191
//   round 2
//   (the nine rows again, the same)
//
// (ALIVE 0x1, RUNNABLE 0x4, WAITING_INDEFINITELY 0x10, WAITING_WITH_TIMEOUT
// 0x20, SLEEPING 0x40, WAITING 0x80, IN_OBJECT_WAIT 0x100, PARKED 0x200,
// BLOCKED_ON_MONITOR_ENTER 0x400.)
//
// CratonVM before wave 57 (code read and wave 56's measurement of the same
// window, L1W56JvmtiReentryContended): no SLEEPING, IN_OBJECT_WAIT or PARKED
// bit (`wait()` 0x91, `wait(ms)` 0xa1, `park` 0x91, `parkNanos` 0xa1,
// `sleep` 0xa1, `join` 0x91), and under `--jdk-only` the `Object.wait` rows
// read RUNNABLE (0x5) while the waiter was still in its lazy running phase,
// as `Thread.getState()` already answered WAITING.
import java.util.concurrent.locks.LockSupport;

public class L1W57JvmtiWaitStates {
    static native String state(String label, Thread t);

    static volatile boolean go;
    static volatile boolean spinning;

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

    static void objectWait(long n, boolean timed) throws InterruptedException {
        Object lock = new Object();
        go = false;
        Thread t = new Thread(() -> {
            synchronized (lock) {
                while (!go) {
                    try {
                        if (timed) {
                            lock.wait(600_000L);
                        } else {
                            lock.wait();
                        }
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "l1w57-wait-" + n);
        t.start();
        await(t, timed ? Thread.State.TIMED_WAITING : Thread.State.WAITING);
        System.out.print(state(timed ? "wait(ms)" : "wait()", t));
        synchronized (lock) {
            go = true;
            lock.notifyAll();
        }
        t.join();
    }

    static void park(long n, boolean timed) throws InterruptedException {
        go = false;
        Thread t = new Thread(() -> {
            while (!go) {
                if (timed) {
                    LockSupport.parkNanos(600_000_000_000L);
                } else {
                    LockSupport.park();
                }
            }
        }, "l1w57-park-" + n);
        t.start();
        await(t, timed ? Thread.State.TIMED_WAITING : Thread.State.WAITING);
        System.out.print(state(timed ? "parkNanos" : "park", t));
        go = true;
        LockSupport.unpark(t);
        t.join();
    }

    static void sleep(long n) throws InterruptedException {
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(600_000L);
            } catch (InterruptedException e) {
                // released
            }
        }, "l1w57-sleep-" + n);
        t.start();
        await(t, Thread.State.TIMED_WAITING);
        System.out.print(state("sleep", t));
        t.interrupt();
        t.join();
    }

    static void join(long n) throws InterruptedException {
        Object lock = new Object();
        go = false;
        Thread target = new Thread(() -> {
            synchronized (lock) {
                while (!go) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "l1w57-target-" + n);
        target.start();
        Thread t = new Thread(() -> {
            try {
                target.join();
            } catch (InterruptedException e) {
                // released
            }
        }, "l1w57-join-" + n);
        t.start();
        await(t, Thread.State.WAITING);
        System.out.print(state("join", t));
        synchronized (lock) {
            go = true;
            lock.notifyAll();
        }
        target.join();
        t.join();
    }

    static void blocked(long n) throws InterruptedException {
        Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                go = true;
            }
        }, "l1w57-blocked-" + n);
        synchronized (lock) {
            t.start();
            await(t, Thread.State.BLOCKED);
            System.out.print(state("blocked", t));
        }
        t.join();
    }

    static void running(long n) throws InterruptedException {
        spinning = true;
        go = false;
        Thread t = new Thread(() -> {
            long x = 0;
            while (spinning) {
                x++;
            }
            go = x >= 0;
        }, "l1w57-running-" + n);
        t.start();
        await(t, Thread.State.RUNNABLE);
        System.out.print(state("running", t));
        spinning = false;
        t.join();
    }

    static void round(long n) throws InterruptedException {
        System.out.println("round " + n);
        objectWait(n, false);
        objectWait(n, true);
        park(n, false);
        park(n, true);
        sleep(n);
        join(n);
        blocked(n);
        running(n);
        objectWait(n, false);
    }

    public static void main(String[] args) throws InterruptedException {
        if (args.length != 1) {
            System.out.println("usage: L1W57JvmtiWaitStates <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
