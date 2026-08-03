import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;

/**
 * Repro for `TestManagerWebapp.testServlets` — `GET /manager/text/threaddump`
 * answers 500 because `org.apache.tomcat.util.Diagnostics.getThreadDump` hits
 *
 *   NullPointerException: Cannot invoke "java.lang.StackTraceElement.toString()"
 *   because the return value of "java.lang.management.MonitorInfo.getLockedStackFrame()" is null
 *
 * `Diagnostics.getThreadDump(ThreadInfo)` does, for each locked monitor:
 *
 *   Object[] monitorDepths = new Object[ti.getStackTrace().length];
 *   monitorDepths[mi.getLockedStackDepth()] = mi;              // (a)
 *   ... mi.getLockedStackFrame().toString()                    // (b)
 *
 * which holds under the JDK's own invariant: a `MonitorInfo` either has
 * `lockedStackDepth >= 0` *and* a non-null `lockedStackFrame`, or has depth -1
 * and a null frame. CratonVM stamped depth 0 on every monitor while leaving the
 * frame null whenever the thread's stack trace came back empty, producing a
 * `MonitorInfo` that satisfies (a) and then explodes at (b).
 *
 * Tomcat's shape is a pool of worker threads that hold monitors while parked,
 * so the probe spawns exactly that: N threads each holding a distinct monitor
 * and blocked inside `Object.wait()`, dumped from a different thread. It also
 * checks the main thread (holding a monitor across its own dump call) and, when
 * tomcat-util is on the classpath, runs the real `Diagnostics.getThreadDump()`.
 *
 * Exits non-zero on any violation; usable as a regression gate on HotSpot and
 * CratonVM alike.
 */
public class ThreadDumpMonitorInfoProbe {

    private static int failures = 0;
    private static final Object MAIN_LOCK = new Object();
    private static final int WORKERS = 4;

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + (detail.isEmpty() ? "" : " — " + detail));
        if (!ok) {
            failures++;
        }
    }

    /** A worker that grabs its own monitor and then parks inside wait(). */
    private static final class Holder implements Runnable {
        private final Object lock = new Object();
        private final CountDownLatch holding = new CountDownLatch(1);
        private volatile boolean done;

        @Override
        public void run() {
            synchronized (lock) {
                holding.countDown();
                while (!done) {
                    try {
                        lock.wait(50);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        return;
                    }
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        ThreadMXBean bean = ManagementFactory.getThreadMXBean();
        check("ThreadMXBean supports object-monitor usage", bean.isObjectMonitorUsageSupported(), "");

        Holder[] holders = new Holder[WORKERS];
        Thread[] workers = new Thread[WORKERS];
        for (int i = 0; i < WORKERS; i++) {
            holders[i] = new Holder();
            workers[i] = new Thread(holders[i], "probe-worker-" + i);
            workers[i].setDaemon(true);
            workers[i].start();
        }
        for (Holder h : holders) {
            h.holding.await();
        }

        ThreadInfo[] infos;
        synchronized (MAIN_LOCK) {
            infos = bean.dumpAllThreads(true, true);
        }
        check("dumpAllThreads returned threads", infos != null && infos.length > 0,
                "got " + (infos == null ? "null" : String.valueOf(infos.length)));

        int monitors = 0;
        int seenWorkers = 0;
        if (infos != null) {
            for (ThreadInfo ti : infos) {
                if (ti == null) {
                    continue;
                }
                if (ti.getThreadName() != null && ti.getThreadName().startsWith("probe-worker-")) {
                    seenWorkers++;
                }
                StackTraceElement[] stes = ti.getStackTrace();
                MonitorInfo[] mis = ti.getLockedMonitors();
                check("getLockedMonitors() is non-null for [" + ti.getThreadName() + "]", mis != null, "");
                if (mis == null) {
                    continue;
                }
                for (MonitorInfo mi : mis) {
                    monitors++;
                    String who = "[" + ti.getThreadName() + "] " + mi.getClassName();
                    int depth = mi.getLockedStackDepth();
                    StackTraceElement frame = mi.getLockedStackFrame();

                    // The JDK invariant Diagnostics depends on.
                    check("depth/frame agree for " + who, (depth >= 0) == (frame != null),
                            "depth=" + depth + " frame=" + frame);

                    if (depth >= 0) {
                        // Diagnostics indexes `new Object[stes.length]` with this.
                        check("depth in stack-trace range for " + who, depth < stes.length,
                                "depth=" + depth + " stackTrace.length=" + stes.length);
                        if (depth < stes.length && frame != null) {
                            check("frame is stackTrace[depth] for " + who,
                                    frame.toString().equals(stes[depth].toString()),
                                    "frame=" + frame + " stackTrace[" + depth + "]=" + stes[depth]);
                        }
                    }
                }
            }
        }
        check("the parked monitor-holding workers appear in the dump", seenWorkers == WORKERS,
                "saw " + seenWorkers + " of " + WORKERS);
        check("at least one locked monitor was reported", monitors > 0, "got " + monitors);

        // The end-to-end shape: exactly what ManagerServlet.threadDump() runs.
        try {
            String dump = org.apache.tomcat.util.Diagnostics.getThreadDump();
            check("Diagnostics.getThreadDump() completes", dump != null && dump.contains("Id="),
                    dump == null ? "null" : "len=" + dump.length());
        } catch (NoClassDefFoundError e) {
            System.out.println("SKIP Diagnostics.getThreadDump() — tomcat-util not on the classpath");
        } catch (Throwable t) {
            check("Diagnostics.getThreadDump() completes", false, t.toString());
        }

        for (int i = 0; i < WORKERS; i++) {
            holders[i].done = true;
            workers[i].interrupt();
        }

        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("ALL OK (" + monitors + " locked monitors checked)");
        System.exit(0);
    }
}
