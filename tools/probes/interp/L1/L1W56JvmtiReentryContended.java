// Interpreter round i2 wave 56, lane L1: the C JVMTI table's
// `GetCurrentContendedMonitor` of a notified waiter re-entering its monitor,
// against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-jvmti-contended-monitor-of-a-re-entering-waiter-is-null-FIXED-20261020.md;
// the C twin of wave 55 L6's JDWP fix, `L6W55JdiSuspendedWaiterReentryTwice`).
//
// The shim's `Agent_OnLoad` adds `can_get_current_contended_monitor`; the
// native `contended` prints, for one thread, `GetCurrentContendedMonitor`
// (null, the lock, or another object) and the `GetThreadState` bits ALIVE,
// RUNNABLE, WAITING and BLOCKED_ON_MONITOR_ENTER. Each round: a thread
// waits on a lock (`waiting`: it contends for nothing), `main` takes the
// lock, notifies, and holds it until the waiter is BLOCKED re-entering
// (`reentering`: it contends for the lock it waited on, JDK-8256314); then a
// second thread blocks entering the lock `main` holds (`entering`, the
// control). Two rounds (the first-call row again).
//
// Needs the native shim tools/probes/interp/L1/L1W56JvmtiReentryContended.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w56reentry.so tools/probes/interp/L1/L1W56JvmtiReentryContended.c
//   javac -d /tmp/l1w56reentry tools/probes/interp/L1/L1W56JvmtiReentryContended.java
//   java     -agentpath:/tmp/libl1w56reentry.so -cp /tmp/l1w56reentry L1W56JvmtiReentryContended /tmp/libl1w56reentry.so
//   cratonvm -agentpath:/tmp/libl1w56reentry.so -cp /tmp/l1w56reentry L1W56JvmtiReentryContended /tmp/libl1w56reentry.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W56JvmtiReentryContended
// (README in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   waiting: onload=0 err=0 monitor=null state=0x81
//   reentering: onload=0 err=0 monitor=lock state=0x401
//   entering: onload=0 err=0 monitor=lock state=0x401
//   round 2
//   (the three rows again, the same)
//
// CratonVM before wave 56 (code read): `reentering: ... monitor=null` (the
// table read only the monitor a `monitorenter` contends for). Since wave 56
// (MEASURED, debug build, Windows): HotSpot's lines under `--compatible`
// (`jvmti::native_env::get_current_contended_monitor`, the waiting monitor
// of a thread BLOCKED re-entering it); under `--jdk-only` the `waiting:` rows
// still read `state=0x5` until wave 57 fixed them
// (docs/internal/fixed-bugs/interpreter-L1-jvmti-thread-state-of-an-object-wait-waiter-is-runnable-under-jdk-only-FIXED-20261021.md).
public class L1W56JvmtiReentryContended {
    static native String contended(String label, Thread t, Object lock);

    static volatile boolean go;

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

    static void round(long n) throws InterruptedException {
        System.out.println("round " + n);
        Object lock = new Object();
        go = false;
        Thread waiter = new Thread(() -> {
            synchronized (lock) {
                while (!go) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "waiter-" + n);
        waiter.start();
        await(waiter, Thread.State.WAITING);
        System.out.print(contended("waiting", waiter, lock));
        Thread enterer = new Thread(() -> {
            synchronized (lock) {
                go = true;
            }
        }, "enterer-" + n);
        synchronized (lock) {
            go = true;
            lock.notifyAll();
            await(waiter, Thread.State.BLOCKED);
            System.out.print(contended("reentering", waiter, lock));
            enterer.start();
            await(enterer, Thread.State.BLOCKED);
            System.out.print(contended("entering", enterer, lock));
        }
        waiter.join();
        enterer.join();
    }

    public static void main(String[] args) throws InterruptedException {
        if (args.length != 1) {
            System.out.println("usage: L1W56JvmtiReentryContended <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
