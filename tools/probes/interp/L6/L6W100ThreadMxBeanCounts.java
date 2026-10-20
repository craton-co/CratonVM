// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- page
// docs/internal/fixed-bugs/interpreter-i100-L6-threadmxbean-counts-and-virtual-threads-FIXED-20261015.md:
// the ThreadMXBean counters and lookups as HotSpot 25 answers them.
//
//  * getTotalStartedThreadCount is cumulative: 40 started-and-joined threads
//    add 40 (CratonVM answered the LIVE count, so the delta was 0);
//  * getPeakThreadCount sees a spike between two reads (CratonVM sampled the
//    live count only when read, so 30 threads that lived and died between
//    two reads never reached the peak); resetPeakThreadCount makes it the
//    live count;
//  * virtual threads are not threads to the JMM: getThreadCount,
//    getDaemonThreadCount and getAllThreadIds leave them out, getThreadInfo
//    answers null and getThreadCpuTime -1 for one, dumpAllThreads lists none
//    (a JDK virtual thread keeps a registry entry for its whole life here,
//    so they were counted and found).
//
// The scheduler is pinned to one carrier (parallelism = maxPoolSize = 1, set
// before the first virtual thread) so that no carrier starts while the
// virtual section measures.
//
// Run (both modes, JIT and --nojit):
//   cratonvm L6W100ThreadMxBeanCounts
//   cratonvm --compatible L6W100ThreadMxBeanCounts
//
// HOTSPOT_EXPECTED_BEGIN
// started-delta=40
// live-restored=true
// peak-saw-spike=true
// peak-reset=true
// virtual-live-unchanged=true
// virtual-daemon-unchanged=true
// ids-exclude-virtual=true
// info-virtual-null=true
// cpu-virtual=-1
// dump-excludes-virtual=true
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L6W100ThreadMxBeanCounts {
    static volatile boolean release;

    public static void main(String[] args) throws Exception {
        System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        System.setProperty("jdk.virtualThreadScheduler.maxPoolSize", "1");
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();

        // 1. Cumulative starts.
        int live0 = mx.getThreadCount();
        long started0 = mx.getTotalStartedThreadCount();
        for (int i = 0; i < 40; i++) {
            Thread t = new Thread(() -> { }, "w100-seq-" + i);
            t.start();
            t.join();
        }
        long started1 = mx.getTotalStartedThreadCount();
        System.out.println("started-delta=" + (started1 - started0));
        System.out.println("live-restored=" + (mx.getThreadCount() == live0));

        // 2. A spike between two reads of the peak.
        mx.resetPeakThreadCount();
        int liveBefore = mx.getThreadCount();
        CountDownLatch ready = new CountDownLatch(30);
        CountDownLatch go = new CountDownLatch(1);
        Thread[] spike = new Thread[30];
        for (int i = 0; i < spike.length; i++) {
            spike[i] = new Thread(() -> {
                ready.countDown();
                try {
                    go.await();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }, "w100-spike-" + i);
            spike[i].start();
        }
        ready.await();
        go.countDown();
        for (Thread t : spike) {
            t.join();
        }
        System.out.println("peak-saw-spike=" + (mx.getPeakThreadCount() >= liveBefore + 30));
        mx.resetPeakThreadCount();
        System.out.println("peak-reset=" + (mx.getPeakThreadCount() == mx.getThreadCount()));

        // 3. Virtual threads. A warm-up one starts the single carrier first.
        Thread.ofVirtual().start(() -> { }).join();
        int liveV = mx.getThreadCount();
        int daemonV = mx.getDaemonThreadCount();
        Thread[] vts = new Thread[20];
        for (int i = 0; i < vts.length; i++) {
            vts[i] = Thread.ofVirtual().name("w100-vt-" + i).start(() -> {
                while (!release) {
                    LockSupport.park();
                }
            });
        }
        for (Thread vt : vts) {
            while (vt.getState() != Thread.State.WAITING) {
                Thread.sleep(1);
            }
        }
        System.out.println("virtual-live-unchanged=" + (mx.getThreadCount() == liveV));
        System.out.println("virtual-daemon-unchanged=" + (mx.getDaemonThreadCount() == daemonV));
        Set<Long> ids = new HashSet<>();
        for (long id : mx.getAllThreadIds()) {
            ids.add(id);
        }
        boolean excluded = true;
        for (Thread vt : vts) {
            excluded &= !ids.contains(vt.threadId());
        }
        System.out.println("ids-exclude-virtual=" + excluded);
        System.out.println("info-virtual-null=" + (mx.getThreadInfo(vts[0].threadId()) == null));
        System.out.println("cpu-virtual=" + mx.getThreadCpuTime(vts[0].threadId()));
        boolean dumpExcludes = true;
        for (ThreadInfo info : mx.dumpAllThreads(false, false)) {
            if (info != null && info.getThreadName().startsWith("w100-vt-")) {
                dumpExcludes = false;
            }
        }
        System.out.println("dump-excludes-virtual=" + dumpExcludes);
        release = true;
        for (Thread vt : vts) {
            LockSupport.unpark(vt);
        }
        for (Thread vt : vts) {
            vt.join();
        }
        System.out.println("done");
    }
}
