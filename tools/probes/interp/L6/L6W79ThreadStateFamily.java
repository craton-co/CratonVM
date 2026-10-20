// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- the Thread.State family of
// docs/known-issues/interpreter/ (a stand-in wait that recorded WAITING where
// HotSpot reads TIMED_WAITING or RUNNABLE):
//   i64-orch-compatible-timed-countdownlatch-await-reads-waiting-20261028.md
//   i66-L1-timed-stand-in-waits-read-waiting-20261030.md
//   i67-L1-process-exit-waiter-reads-waiting-20261031.md
// (i66-L1-second-exit-caller-reads-waiting-not-blocked: L6W79SecondExitCaller;
//  i65-L1-virtual-thread-unblocker-reads-timed-waiting: the thread was absent;
//  present and RUNNABLE since 2026-10-10, FIXED.)
//
// Rows: a timed CountDownLatch.await; a Cleaner's daemon in its 60 s
// ReferenceQueue.remove (JDK 25: `lock.wait(timeout)` on ReferenceQueue$Lock);
// a timed ScheduledExecutorService.awaitTermination (`awaitNanos`, a timed
// park); the thread that waits for a child process after Process.onExit()
// (HotSpot: the `process reaper (pid N)` pool thread, RUNNABLE in the native
// waitForProcessExit0). Linux host (`sleep 3` child).
//
// Run, each mode:
//   cratonvm L6W79ThreadStateFamily
//   cratonvm --compatible L6W79ThreadStateFamily
//
// Under --compatible the reaper is CratonVM's own stand-in thread; since wave
// 80 (lane L6) it is named `process reaper (pid N)` as HotSpot's (it read
// `process-reaper-<h>`, so `named-like-hotspot=false`, before).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// CountDownLatch.await(ms): TIMED_WAITING jmx-same=true
// Cleaner queue.remove(60s): TIMED_WAITING jmx-same=true lock=java.lang.ref.ReferenceQueue$Lock
// ScheduledExecutorService.awaitTermination: TIMED_WAITING jmx-same=true
// process reaper: RUNNABLE in-native=true named-like-hotspot=true
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.lang.ref.Cleaner;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

public class L6W79ThreadStateFamily {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Blocking {
        void run() throws Exception;
    }

    static String lockClass(ThreadInfo info) {
        if (info == null || info.getLockName() == null) {
            return "null";
        }
        String name = info.getLockName();
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    /** Waits until `t` reads `want` (or 5 s), then prints its state and JMX's. */
    static void report(String label, Thread t, Thread.State want, boolean showLock) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (t.getState() != want && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
        Thread.State s = t.getState();
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50 && (info == null || info.getThreadState() != s); i++) {
            Thread.sleep(2);
            s = t.getState();
            info = MX.getThreadInfo(t.threadId());
        }
        boolean same = info != null && info.getThreadState() == s;
        System.out.println(label + ": " + s + " jmx-same=" + same
                + (showLock ? " lock=" + lockClass(info) : ""));
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
        report(label, t, Thread.State.TIMED_WAITING, false);
        t.interrupt();
        t.join(2000);
    }

    static Thread reaper() {
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            String n = t.getName();
            if (n.startsWith("process reaper") || n.startsWith("process-reaper")) {
                if (t.getState() == Thread.State.RUNNABLE || n.contains("pid")
                        || n.startsWith("process-reaper")) {
                    return t;
                }
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        CountDownLatch latch = new CountDownLatch(1);
        row("CountDownLatch.await(ms)", () -> latch.await(60, TimeUnit.SECONDS));

        Cleaner cleaner = Cleaner.create(r -> {
            Thread t = new Thread(r, "w79-cleaner");
            t.setDaemon(true);
            return t;
        });
        Thread cleanerThread = null;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getName().equals("w79-cleaner")) {
                cleanerThread = t;
            }
        }
        if (cleanerThread == null) {
            System.out.println("Cleaner queue.remove(60s): absent");
        } else {
            report("Cleaner queue.remove(60s)", cleanerThread, Thread.State.TIMED_WAITING, true);
        }

        ScheduledExecutorService ses = Executors.newScheduledThreadPool(1);
        row("ScheduledExecutorService.awaitTermination",
                () -> ses.awaitTermination(60, TimeUnit.SECONDS));
        ses.shutdownNow();

        Process p = new ProcessBuilder("sleep", "3").start();
        p.onExit();
        Thread r = null;
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2);
        while ((r = reaper()) == null && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        if (r == null) {
            System.out.println("process reaper: absent");
        } else {
            Thread.sleep(100);
            ThreadInfo info = MX.getThreadInfo(r.threadId());
            System.out.println("process reaper: " + r.getState()
                    + " in-native=" + (info != null && info.isInNative())
                    + " named-like-hotspot=" + r.getName().startsWith("process reaper (pid "));
        }
        p.destroyForcibly();
        p.waitFor();
        System.out.println("done");
        java.lang.ref.Reference.reachabilityFence(cleaner);
    }
}
