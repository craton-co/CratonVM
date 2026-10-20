// Interpreter round i2 wave 63, lane L1: the C JVMTI table's `GetThreadState`
// (17) of a thread in `Thread.sleep`, entered through every door, and of a
// thread that waits (timed) right after a sleep ended, against HotSpot. Stage 2
// of docs/internal/fixed-bugs/interpreter-i58-L1-proposal-one-published-thread-state-word-DONE-20261104.md
// (switch `CRATONVM_PUBLISHED_WAIT_KIND`).
//
// HotSpot's `SLEEPING` bit comes from the thread's own `JavaThreadSleepState`,
// so a sleep reads 0xe1 whatever frame entered it. CratonVM before wave 63
// read the bit from the deposited top frame (`jvmti::native_env::wait_kind`:
// a `Thread.sleep*` frame, or a frame stopped at an invoke of one), so a sleep
// entered through `Method.invoke`, a method handle or `TimeUnit.sleep` (all
// served by registered natives under `--compatible`, with no `Thread.sleep*`
// frame and no invoke of one on top) read PARKED, 0x2a1 (code reading; the
// orchestrator measures the before/after rows).
//
// Each row starts a thread, waits until `Thread.getState()` answers
// TIMED_WAITING, asks at once, then interrupts (or unparks / notifies) the
// thread and joins it. Two rounds.
//
// Needs the native shim tools/probes/interp/L1/L1W63JvmtiSleepKinds.c, loaded
// both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63sleep.so tools/probes/interp/L1/L1W63JvmtiSleepKinds.c
//   javac -d /tmp/l1w63sleep tools/probes/interp/L1/L1W63JvmtiSleepKinds.java
//   java     -agentpath:/tmp/libl1w63sleep.so -cp /tmp/l1w63sleep L1W63JvmtiSleepKinds /tmp/libl1w63sleep.so
//   CRATONVM_PUBLISHED_WAIT_KIND=1 cratonvm --compatible \
//            -agentpath:/tmp/libl1w63sleep.so -cp /tmp/l1w63sleep L1W63JvmtiSleepKinds /tmp/libl1w63sleep.so
//   (and again with --jdk-only, and with the switch off for the before rows)
//
// Without the argument it prints only the usage line.
//
// (ALIVE 0x1, WAITING_WITH_TIMEOUT 0x20, SLEEPING 0x40, WAITING 0x80,
// IN_OBJECT_WAIT 0x100, PARKED 0x200.)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// round 1
// sleep(ms): err=0 state=0xe1
// sleep(ms,ns): err=0 state=0xe1
// sleep(Duration): err=0 state=0xe1
// TimeUnit.sleep: err=0 state=0xe1
// Method.invoke(sleep): err=0 state=0xe1
// MethodHandle(sleep): err=0 state=0xe1
// parkNanos after sleep: err=0 state=0x2a1
// wait(ms) after sleep: err=0 state=0x1a1
// round 2
// sleep(ms): err=0 state=0xe1
// sleep(ms,ns): err=0 state=0xe1
// sleep(Duration): err=0 state=0xe1
// TimeUnit.sleep: err=0 state=0xe1
// Method.invoke(sleep): err=0 state=0xe1
// MethodHandle(sleep): err=0 state=0xe1
// parkNanos after sleep: err=0 state=0x2a1
// wait(ms) after sleep: err=0 state=0x1a1
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.time.Duration;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L1W63JvmtiSleepKinds {
    static native String state(String label, Thread t);

    static final long LONG_MS = 600_000L;

    static volatile boolean go;
    static volatile boolean slept;

    interface Sleeper {
        void sleep() throws Throwable;
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

    static void sleepRow(long n, String label, Sleeper sleeper) throws InterruptedException {
        Thread t = new Thread(() -> {
            try {
                sleeper.sleep();
            } catch (Throwable e) {
                // released by the interrupt
            }
        }, "l1w63-" + label + "-" + n);
        t.start();
        await(t, Thread.State.TIMED_WAITING);
        System.out.print(state(label, t));
        t.interrupt();
        t.join();
    }

    static void parkAfterSleep(long n) throws InterruptedException {
        go = false;
        slept = false;
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(1);
            } catch (InterruptedException e) {
                return;
            }
            slept = true;
            while (!go) {
                LockSupport.parkNanos(LONG_MS * 1_000_000L);
            }
        }, "l1w63-park-" + n);
        t.start();
        while (!slept) {
            Thread.sleep(1);
        }
        await(t, Thread.State.TIMED_WAITING);
        System.out.print(state("parkNanos after sleep", t));
        go = true;
        LockSupport.unpark(t);
        t.join();
    }

    static void waitAfterSleep(long n) throws InterruptedException {
        Object lock = new Object();
        go = false;
        slept = false;
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(1);
            } catch (InterruptedException e) {
                return;
            }
            slept = true;
            synchronized (lock) {
                while (!go) {
                    try {
                        lock.wait(LONG_MS);
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "l1w63-wait-" + n);
        t.start();
        while (!slept) {
            Thread.sleep(1);
        }
        await(t, Thread.State.TIMED_WAITING);
        System.out.print(state("wait(ms) after sleep", t));
        synchronized (lock) {
            go = true;
            lock.notifyAll();
        }
        t.join();
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        sleepRow(n, "sleep(ms)", () -> Thread.sleep(LONG_MS));
        sleepRow(n, "sleep(ms,ns)", () -> Thread.sleep(LONG_MS, 1));
        sleepRow(n, "sleep(Duration)", () -> Thread.sleep(Duration.ofMillis(LONG_MS)));
        sleepRow(n, "TimeUnit.sleep", () -> TimeUnit.MILLISECONDS.sleep(LONG_MS));
        Method reflective = Thread.class.getMethod("sleep", long.class);
        sleepRow(n, "Method.invoke(sleep)", () -> reflective.invoke(null, LONG_MS));
        MethodHandle handle = MethodHandles.lookup()
                .findStatic(Thread.class, "sleep", MethodType.methodType(void.class, long.class));
        sleepRow(n, "MethodHandle(sleep)", () -> {
            handle.invokeExact(LONG_MS);
        });
        parkAfterSleep(n);
        waitAfterSleep(n);
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiSleepKinds <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
