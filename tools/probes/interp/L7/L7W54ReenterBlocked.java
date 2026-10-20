// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L7: a waiter whose `Object.wait` has
// ended (notified, interrupted, or timed out) and that is re-entering a
// monitor another thread still holds is BLOCKED, through both answers:
// `Thread.getState()` (the JDK's own `threadState()` reads
// `holder.threadStatus`) and `ThreadMXBean.getThreadInfo` (the VM's thread
// registry). Page
// `docs/internal/fixed-bugs/interpreter-L7-a-notified-waiter-re-entering-a-held-monitor-reports-waiting-FIXED-20261018.md`:
// after SBR round 2 retired the `getState` native, the `threadStatus` word
// kept the WAITING the wait's deposit wrote for the whole re-acquire, while
// only the registry word turned BLOCKED (`WaitReenterStatus` now writes both).
//
//   notify    wait(), notified, monitor held 300 ms
//   interrupt wait(), interrupted, monitor held 300 ms
//   timeout   wait(50), timed out while the monitor is held
//   timed     wait(600000), notified, monitor held
//   after     the state once the holder released and the waiter left
//
// The `notify` row runs first (cold VM) and again last.
//
// Expected (HotSpot 25.0.3, `java L7W54ReenterBlocked`, default flags and
// -Xint, three runs each):
//   notify: getState=BLOCKED info=BLOCKED log=returned flag=false after=TERMINATED
//   interrupt: getState=BLOCKED info=BLOCKED log=IE flag=false after=TERMINATED
//   timeout: getState=BLOCKED info=BLOCKED log=returned flag=false after=TERMINATED
//   timed: getState=BLOCKED info=BLOCKED log=returned flag=false after=TERMINATED
//   notify: getState=BLOCKED info=BLOCKED log=returned flag=false after=TERMINATED

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public class L7W54ReenterBlocked {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long end = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != s && System.nanoTime() < end) Thread.sleep(2);
        Thread.sleep(50);
    }

    static void row(String name) throws Exception {
        Object m = new Object();
        StringBuffer log = new StringBuffer();
        long timeout = switch (name) {
            case "timeout" -> 50L;
            case "timed" -> 600_000L;
            default -> 0L;
        };
        Thread a = new Thread(() -> {
            synchronized (m) {
                try {
                    if (timeout > 0) m.wait(timeout); else m.wait();
                    log.append("returned flag=").append(Thread.currentThread().isInterrupted());
                } catch (InterruptedException e) {
                    log.append("IE flag=").append(Thread.currentThread().isInterrupted());
                }
            }
        }, "a-" + name);
        Thread.State s;
        String info;
        if (name.equals("timeout")) {
            // Take the monitor as soon as the waiter is in its wait, and keep
            // it past the 50 ms timeout.
            a.start();
            awaitStateNoSettle(a, Thread.State.TIMED_WAITING);
            synchronized (m) {
                Thread.sleep(300);
                s = a.getState();
                info = String.valueOf(MX.getThreadInfo(a.threadId()).getThreadState());
            }
        } else {
            a.start();
            awaitState(a, timeout > 0 ? Thread.State.TIMED_WAITING : Thread.State.WAITING);
            synchronized (m) {
                if (name.equals("interrupt")) a.interrupt(); else m.notify();
                Thread.sleep(300);
                s = a.getState();
                info = String.valueOf(MX.getThreadInfo(a.threadId()).getThreadState());
            }
        }
        a.join();
        System.out.println(name + ": getState=" + s + " info=" + info + " log=" + log + " after=" + a.getState());
    }

    static void awaitStateNoSettle(Thread t, Thread.State s) throws InterruptedException {
        long end = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != s && System.nanoTime() < end) Thread.onSpinWait();
    }

    public static void main(String[] args) throws Exception {
        for (String name : new String[] {"notify", "interrupt", "timeout", "timed", "notify"}) {
            row(name);
        }
    }
}
