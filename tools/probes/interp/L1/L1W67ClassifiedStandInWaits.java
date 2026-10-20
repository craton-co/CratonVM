// Interpreter round i2 wave 67, lane L1
// (docs/internal/fixed-bugs/interpreter-i66-L1-proposal-ratchet-the-unrecorded-wait-census-DONE-20261030.md):
// the Java-visible state of a thread in each stand-in wait the wave-66
// unrecorded-wait census listed, read without an agent, plus its JMX lock
// class. `CRATONVM_CLASSIFIED_STAND_IN_WAITS` (default on, `=0` kill switch)
// makes each region say the kind of the JDK 25 code it stands for:
//
// * `ReferenceQueue.remove()` / `remove(ms)`: JDK 25 waits on a plain
//   `ReferenceQueue$Lock` monitor (`lock.wait()` / `lock.wait(timeout)`), so the
//   timed one is TIMED_WAITING; CratonVM's poll read WAITING for both, and so
//   did the `--jdk-only` lock wait. `Common-Cleaner` sits in `remove(60000)`.
// * `Process.waitFor()` / `waitFor(ms)` (UNIX): JDK 25's `ProcessImpl` awaits a
//   `ReentrantLock` condition (host-measured lock
//   `AbstractQueuedSynchronizer$ConditionObject`): WAITING / TIMED_WAITING,
//   JVMTI PARKED. The timed one read WAITING.
// * `Process.waitFor(ms)` of a `Process` subclass runs the default body, which
//   loops on `Thread.sleep`: TIMED_WAITING, no lock. The stand-in read WAITING.
//
// The `lock=` columns of the queue and process rows are a known gap (a
// blocking region publishes no waiting monitor;
// `i66-L1-process-waitfor-names-no-lock-20261030.md`,
// `i67-L1-stand-in-regions-name-no-jmx-lock-20261031.md`): expect `lock=null`
// from CratonVM where the wait is a region rather than a monitor wait.
//
// Census: `CRATONVM_DBG_UNRECORDED_WAIT_KINDS=1 cratonvm --compatible
// L1W67ClassifiedStandInWaits` must not list `reference.rs`,
// `native-collections` or the `process.rs` `waitFor` sites; with
// `CRATONVM_CLASSIFIED_STAND_IN_WAITS=0` it lists them again.
//
// Run in both modes: `cratonvm L1W67ClassifiedStandInWaits` (jdk-only) and
// `cratonvm --compatible L1W67ClassifiedStandInWaits`; both must print the
// block below. `CRATONVM_CLASSIFIED_STAND_IN_WAITS=0` is expected to fall back
// to WAITING on the timed rows.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// ReferenceQueue.remove(): WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock
// ReferenceQueue.remove(ms): TIMED_WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock
// Common-Cleaner: TIMED_WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock
// Process.waitFor(): WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// Process.waitFor(ms): TIMED_WAITING jmx-same=true lock=java.util.concurrent.locks.AbstractQueuedSynchronizer$ConditionObject
// Process subclass waitFor(ms): TIMED_WAITING jmx-same=true lock=null
// done
// HOTSPOT_EXPECTED_END
import java.io.File;
import java.io.FileInputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.lang.ref.ReferenceQueue;
import java.util.concurrent.TimeUnit;

public class L1W67ClassifiedStandInWaits {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Blocking {
        void run() throws Exception;
    }

    /** A `Process` that never exits, so `waitFor(long, TimeUnit)` runs `Process`'s own body. */
    static final class Foreign extends Process {
        @Override
        public OutputStream getOutputStream() {
            return OutputStream.nullOutputStream();
        }

        @Override
        public InputStream getInputStream() {
            return InputStream.nullInputStream();
        }

        @Override
        public InputStream getErrorStream() {
            return InputStream.nullInputStream();
        }

        @Override
        public int waitFor() throws InterruptedException {
            Thread.sleep(Long.MAX_VALUE);
            return 0;
        }

        @Override
        public int exitValue() {
            throw new IllegalThreadStateException("running");
        }

        @Override
        public void destroy() {
        }
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

    /** Sample `t` until it waits and JMX agrees, then print its row. */
    static void report(String label, Thread t) throws Exception {
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
        System.out.println(label + ": " + s + " jmx-same=" + same + " lock=" + lockClass(info));
    }

    static void row(String label, Blocking body) throws Exception {
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
        report(label, t);
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
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        row("ReferenceQueue.remove()", () -> q.remove());
        row("ReferenceQueue.remove(ms)", () -> q.remove(60_000));

        // A FileInputStream registers a FileCleanable with the common cleaner,
        // which starts the `Common-Cleaner` thread if nothing did yet.
        File f = File.createTempFile("l1w67", ".tmp");
        f.deleteOnExit();
        new FileInputStream(f).close();
        Thread cleaner = named("Common-Cleaner");
        if (cleaner == null) {
            System.out.println("Common-Cleaner: absent");
        } else {
            report("Common-Cleaner", cleaner);
        }

        Process p = new ProcessBuilder("sleep", "30").start();
        row("Process.waitFor()", () -> p.waitFor());
        row("Process.waitFor(ms)", () -> p.waitFor(60, TimeUnit.SECONDS));
        p.destroyForcibly();
        p.waitFor(5, TimeUnit.SECONDS);

        Foreign foreign = new Foreign();
        row("Process subclass waitFor(ms)", () -> foreign.waitFor(60, TimeUnit.SECONDS));
        System.out.println("done");
    }
}
