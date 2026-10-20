// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe
//
// Interpreter round i6 wave 133, lane L7: `CRATONVM_THREADS_ATTACH_ORDER` (HotSpot parity,
// default ON, `=0` off), item 5 of
// docs/internal/proposals/interpreter/i102-L1-proposal-debugger-and-process-directions-20261204.md.
// HotSpot answers every "all live threads" question by walking its threads list
// (`ThreadsListHandle`), where a thread is appended when it attaches and an exited thread's slot
// is compacted away in order. So `Thread.getThreads()` (behind `ThreadGroup.enumerate`,
// `Thread.getAllStackTraces`) and `ThreadImpl.getThreads()` / `dumpThreads0` (behind
// `ThreadMXBean.getAllThreadIds` / `dumpAllThreads`) list threads in attach order. CratonVM's
// `NativeContextImpl::enumerate_threads` read the thread registry's hash map in its hash order.
//
// Rows (only this probe's threads and `main`, so the VM's own service threads do not matter;
// each worker is started only after the previous one is running):
//
//   ids        `ThreadMXBean.getAllThreadIds()` mapped to names
//   dump       `ThreadMXBean.dumpAllThreads(false, false)` names
//   enumerate  `main`'s group `enumerate(Thread[], true)` names
//   group      a child group's `enumerate` (its own two threads)
//   after-exit the ids row after w133-c exited and w133-h started (h goes last, c's slot is gone)
//
//   javac -d <dir> tools/probes/interp/L7/L7W133ThreadListOrder.java
//   java -cp <dir> L7W133ThreadListOrder
//   java -Xint -cp <dir> L7W133ThreadListOrder
//   cratonvm --java-home <jdk25> [--compatible] [--nojit] -cp <dir> L7W133ThreadListOrder
//   CRATONVM_THREADS_ATTACH_ORDER=0 cratonvm ... L7W133ThreadListOrder   (control: the hash order,
//                                                                          expected to DIFF)
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// ids main w133-a w133-b w133-c w133-d w133-e w133-f w133-g1 w133-g2
// dump main w133-a w133-b w133-c w133-d w133-e w133-f w133-g1 w133-g2
// enumerate main w133-a w133-b w133-c w133-d w133-e w133-f w133-g1 w133-g2
// group w133-g1 w133-g2
// after-exit main w133-a w133-b w133-d w133-e w133-f w133-g1 w133-g2 w133-h
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;

public class L7W133ThreadListOrder {
    static final CountDownLatch release = new CountDownLatch(1);

    static Thread start(ThreadGroup group, String name, CountDownLatch until) throws Exception {
        CountDownLatch running = new CountDownLatch(1);
        Thread t = new Thread(group, () -> {
            running.countDown();
            try {
                until.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }, name);
        t.setDaemon(true);
        t.start();
        running.await();
        return t;
    }

    static boolean ours(String name) {
        return name != null && (name.equals("main") || name.startsWith("w133-"));
    }

    static String idsRow(ThreadMXBean mx) {
        StringBuilder sb = new StringBuilder();
        for (long id : mx.getAllThreadIds()) {
            ThreadInfo info = mx.getThreadInfo(id);
            if (info != null && ours(info.getThreadName())) {
                sb.append(' ').append(info.getThreadName());
            }
        }
        return sb.toString();
    }

    static String dumpRow(ThreadMXBean mx) {
        StringBuilder sb = new StringBuilder();
        for (ThreadInfo info : mx.dumpAllThreads(false, false)) {
            if (info != null && ours(info.getThreadName())) {
                sb.append(' ').append(info.getThreadName());
            }
        }
        return sb.toString();
    }

    static String enumerateRow(ThreadGroup group) {
        Thread[] all = new Thread[group.activeCount() + 64];
        int n = group.enumerate(all, true);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) {
            if (ours(all[i].getName())) {
                sb.append(' ').append(all[i].getName());
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadGroup mainGroup = Thread.currentThread().getThreadGroup();
        CountDownLatch cDone = new CountDownLatch(1);
        start(mainGroup, "w133-a", release);
        start(mainGroup, "w133-b", release);
        Thread c = start(mainGroup, "w133-c", cDone);
        start(mainGroup, "w133-d", release);
        start(mainGroup, "w133-e", release);
        start(mainGroup, "w133-f", release);
        ThreadGroup child = new ThreadGroup(mainGroup, "w133-group");
        start(child, "w133-g1", release);
        start(child, "w133-g2", release);

        System.out.println("ids" + idsRow(mx));
        System.out.println("dump" + dumpRow(mx));
        System.out.println("enumerate" + enumerateRow(mainGroup));
        System.out.println("group" + enumerateRow(child));

        cDone.countDown();
        c.join();
        start(mainGroup, "w133-h", release);
        System.out.println("after-exit" + idsRow(mx));
        release.countDown();
    }
}
