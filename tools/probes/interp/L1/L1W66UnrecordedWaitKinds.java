// Interpreter round i2 wave 66, lane L1
// (docs/internal/fixed-bugs/interpreter-i65-L1-proposal-unrecorded-wait-kind-as-a-debug-assertion-DONE-20261029.md):
// the Java-visible state of the threads whose blocking regions this wave
// classified, read without an agent, plus the common recorded waits so a
// census run reaches them.
//
// Rows this wave changes (`CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`, default on):
// the `Reference Handler` thread sits in the JDK native
// `Reference.waitForReferencePendingList`, which HotSpot runs inside the VM
// (`_thread_blocked` under `Heap_lock`): RUNNABLE, JMX RUNNABLE, not in
// native. CratonVM's stand-in sleeps in a blocking region and read WAITING.
// `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE=0` brings the old answer back.
//
// `Process.waitFor`: wave 66 read the UNIX `ProcessImpl.waitFor` as
// `synchronized ... wait()`; the host run (JDK 25.0.4) measured it WAITING on
// a `ReentrantLock` condition (`AbstractQueuedSynchronizer$ConditionObject`),
// so it PARKS. Wave 67 (lane L1, `CRATONVM_CLASSIFIED_STAND_IN_WAITS`) makes
// the region say `BlockKind::Parked` (JVMTI PARKED under
// `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`; needs an agent). Its `lock=` column is
// the JMX lock class, which HotSpot names and CratonVM does not yet
// (`i66-L1-process-waitfor-names-no-lock-20261030.md`). Expected row corrected
// to the measured line in wave 67.
//
// The census: `CRATONVM_DBG_UNRECORDED_WAIT_KINDS=1 cratonvm L1W66UnrecordedWaitKinds`
// prints, on stderr at exit, every blocking region that opened with no
// recorded wait kind, by the Rust call site of its door
// (`[cratonvm] unrecorded-wait-kinds: R regions at S sites`); `=trace` also
// prints each site when first reached. The recorded rows below (sleep,
// Object.wait, park, join) must not appear in it. stdout is unchanged by it.
//
// Run in both modes: `cratonvm L1W66UnrecordedWaitKinds` (jdk-only) and
// `cratonvm --compatible L1W66UnrecordedWaitKinds`; both must print the block
// below.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// Reference Handler: RUNNABLE jmx=RUNNABLE in-native=false
// Thread.sleep(ms): TIMED_WAITING jmx-same=true
// Object.wait(): WAITING jmx-same=true
// Object.wait(ms): TIMED_WAITING jmx-same=true
// LockSupport.park: WAITING jmx-same=true
// LockSupport.parkNanos: TIMED_WAITING jmx-same=true
// Thread.join(): WAITING jmx-same=true
// Thread.join(ms): TIMED_WAITING jmx-same=true
// Process.waitFor: WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L1W66UnrecordedWaitKinds {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Blocking {
        void run() throws Exception;
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String lockClass(ThreadInfo info) {
        if (info == null || info.getLockName() == null) {
            return "null";
        }
        String name = info.getLockName();
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    static void row(String label, boolean showLock, Blocking body) throws Exception {
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
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
        Thread.State s = t.getState();
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            if (waiting(s) && info != null && info.getThreadState() == s) {
                break;
            }
            Thread.sleep(2);
            s = t.getState();
            info = MX.getThreadInfo(t.threadId());
        }
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same
                + (showLock ? " lock=" + lockClass(info) : ""));
        t.interrupt();
        t.join(2000);
    }

    static Thread named(String name) {
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getName().equals(name)) {
                return t;
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        Thread handler = named("Reference Handler");
        if (handler == null) {
            System.out.println("Reference Handler: absent");
        } else {
            // Settled: the handler is idle once startup's references are drained.
            Thread.sleep(100);
            Thread.State s = handler.getState();
            ThreadInfo info = MX.getThreadInfo(handler.threadId());
            System.out.println("Reference Handler: " + s
                    + " jmx=" + (info == null ? "null" : info.getThreadState())
                    + " in-native=" + (info != null && info.isInNative()));
        }

        row("Thread.sleep(ms)", false, () -> Thread.sleep(60_000));
        Object m = new Object();
        row("Object.wait()", false, () -> {
            synchronized (m) {
                m.wait();
            }
        });
        row("Object.wait(ms)", false, () -> {
            synchronized (m) {
                m.wait(60_000);
            }
        });
        row("LockSupport.park", false, () -> {
            LockSupport.park();
            if (Thread.interrupted()) {
                throw new InterruptedException();
            }
        });
        row("LockSupport.parkNanos", false, () -> {
            LockSupport.parkNanos(TimeUnit.SECONDS.toNanos(60));
            if (Thread.interrupted()) {
                throw new InterruptedException();
            }
        });

        Thread sleeper = new Thread(() -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                // Interrupted at the end.
            }
        }, "joined");
        sleeper.setDaemon(true);
        sleeper.start();
        row("Thread.join()", false, () -> sleeper.join());
        row("Thread.join(ms)", false, () -> sleeper.join(60_000));
        sleeper.interrupt();
        sleeper.join(2000);

        Process p = new ProcessBuilder("sleep", "30").start();
        row("Process.waitFor", true, () -> p.waitFor());
        p.destroyForcibly();
        p.waitFor(5, TimeUnit.SECONDS);
        System.out.println("done");
    }
}
