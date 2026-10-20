// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L7: which stack frame
// `ThreadInfo.getLockedMonitors()` attributes each held monitor to
// (`MonitorInfo.getLockedStackDepth()` / `getLockedStackFrame()`), for the
// calling thread and for other threads parked in `Thread.sleep` and in
// `Object.wait`, through `getThreadInfo(ids, true, true)` and
// `dumpAllThreads(true, true)`, printed jstack-style ("- locked <class> at depth N in
// <method>"). Proposal `i23-L7-proposal-per-frame-locked-monitors`, stage 1.
//
// Shapes: a synchronized method (`outer`, locks `A`), a synchronized block in
// it (`B`), a block in a callee (`middle`, `C`), the same object `A`
// re-entered by a block in a deeper callee (`inner`), and a waiting thread
// whose waited-on monitor must NOT be reported (it is released while
// waiting). Only frames of the probe's classes are printed, with their depth relative
// to the innermost probe frame (`inner` / `waitInner`), so the JDK and lambda
// frames above it do not matter.
//
// Run: cratonvm --java-home <jdk25> --nojit [--compatible] -cp <dir> L7W24LockedMonitorDepths
// (`--nojit`: a JIT-compiled frame's monitors are not attributed to their
// frame yet -- stage 2 of the proposal.)
//
// HotSpot 25 prints exactly:
//     self: 4 monitors
//       - locked L7W24LockedMonitorDepths$A at inner (depth 0)
//       - locked L7W24LockedMonitorDepths$C at middle (depth 1)
//       - locked L7W24LockedMonitorDepths$B at outer (depth 2)
//       - locked L7W24LockedMonitorDepths$A at outer (depth 2)
//     sleeper: 4 monitors
//       - locked L7W24LockedMonitorDepths$A at inner (depth 0)
//       - locked L7W24LockedMonitorDepths$C at middle (depth 1)
//       - locked L7W24LockedMonitorDepths$B at outer (depth 2)
//       - locked L7W24LockedMonitorDepths$A at outer (depth 2)
//     dumpAllThreads sleeper: 4 monitors
//       - locked L7W24LockedMonitorDepths$A at inner (depth 0)
//       - locked L7W24LockedMonitorDepths$C at middle (depth 1)
//       - locked L7W24LockedMonitorDepths$B at outer (depth 2)
//       - locked L7W24LockedMonitorDepths$A at outer (depth 2)
//     waiter: 1 monitors
//       - locked L7W24LockedMonitorDepths$B at waitOuter (depth 1)
//     waiter lock L7W24LockedMonitorDepths$C
//
// CratonVM before wave 24 (read from the code): every monitor was attributed
// to the innermost frame of the whole stack (depth 0, a JDK frame), each
// object once, in acquisition order; the waiter also listed the monitor it
// waits on.
import java.lang.management.ManagementFactory;
import java.lang.management.MonitorInfo;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L7W24LockedMonitorDepths {
    static final class A {
        // A synchronized METHOD (ACC_SYNCHRONIZED: the frame's method
        // monitor), holding a block monitor too.
        synchronized void outer(Leaf leaf) throws Exception {
            synchronized (b) {
                middle(leaf);
            }
        }
    }

    static final class B {}
    static final class C {}

    static final A a = new A();
    static final B b = new B();
    static final C c = new C();
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Leaf {
        void run() throws Exception;
    }

    static void middle(Leaf leaf) throws Exception {
        synchronized (c) {
            inner(leaf);
        }
    }

    static void inner(Leaf leaf) throws Exception {
        synchronized (a) {
            leaf.run();
        }
    }

    static void waitOuter(CountDownLatch ready) throws Exception {
        synchronized (b) {
            waitInner(ready);
        }
    }

    static void waitInner(CountDownLatch ready) throws Exception {
        synchronized (c) {
            ready.countDown();
            c.wait();
        }
    }

    static void report(String label, ThreadInfo ti) {
        StackTraceElement[] st = ti.getStackTrace();
        int base = -1;
        for (int i = 0; i < st.length; i++) {
            if (st[i].getClassName().equals(L7W24LockedMonitorDepths.class.getName())
                    && (st[i].getMethodName().equals("inner") || st[i].getMethodName().equals("waitInner"))) {
                base = i;
                break;
            }
        }
        List<String> lines = new ArrayList<>();
        for (MonitorInfo mi : ti.getLockedMonitors()) {
            int d = mi.getLockedStackDepth();
            StackTraceElement f = mi.getLockedStackFrame();
            if (f == null || d < 0 || !f.getClassName().startsWith(L7W24LockedMonitorDepths.class.getName())) {
                continue;
            }
            lines.add("  - locked " + mi.getClassName() + " at " + f.getMethodName()
                    + " (depth " + (d - base) + ")");
        }
        System.out.println(label + ": " + lines.size() + " monitors");
        lines.forEach(System.out::println);
    }

    public static void main(String[] args) throws Exception {
        a.outer(() -> {
            ThreadInfo ti = MX.getThreadInfo(new long[] {Thread.currentThread().threadId()}, true, true)[0];
            report("self", ti);
        });

        CountDownLatch sleeping = new CountDownLatch(1);
        Thread sleeper = new Thread(() -> {
            try {
                a.outer(() -> {
                    sleeping.countDown();
                    Thread.sleep(60_000);
                });
            } catch (Exception e) {
            }
        });
        sleeper.setDaemon(true);
        sleeper.start();
        sleeping.await();
        waitFor(sleeper, Thread.State.TIMED_WAITING);
        report("sleeper", MX.getThreadInfo(new long[] {sleeper.threadId()}, true, true)[0]);
        for (ThreadInfo t : MX.dumpAllThreads(true, true)) {
            if (t.getThreadId() == sleeper.threadId()) {
                report("dumpAllThreads sleeper", t);
            }
        }

        CountDownLatch ready = new CountDownLatch(1);
        Thread waiter = new Thread(() -> {
            try {
                waitOuter(ready);
            } catch (Exception e) {
            }
        });
        waiter.setDaemon(true);
        waiter.start();
        ready.await();
        waitFor(waiter, Thread.State.WAITING);
        ThreadInfo wi = MX.getThreadInfo(new long[] {waiter.threadId()}, true, true)[0];
        report("waiter", wi);
        System.out.println("waiter lock " + (wi.getLockInfo() == null ? null : wi.getLockInfo().getClassName()));
    }

    static void waitFor(Thread t, Thread.State s) throws Exception {
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != s && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
    }
}
