// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4 (threads, L7 probe prefix): how an
// `Object.wait()` ends, and what `Thread.getState()` says meanwhile.
//
//   1-2  `notify()` and then `interrupt()` of the same waiter, both inside the
//        notifier's `synchronized` block. HotSpot's waiter was notified
//        (`WasNotified`), so it returns normally and keeps its interrupt
//        pending. CratonVM's `Monitor::wait` consumed the notification credit
//        and returned, and `monitor_wait_with_state` then saw the flag and
//        threw InterruptedException: the notification was consumed AND the
//        waiter threw, a lost wakeup when another thread waits on the same
//        monitor (JLS 17.2.4). Now `Monitor::wait` reports a `WaitOutcome`
//        and a `Notified` wait returns normally.
//   3-4  a waiter that was notified (3) or interrupted (4) and is re-entering
//        a monitor the main thread still holds is BLOCKED. CratonVM kept the
//        WAITING it stored before the park for the whole re-acquire.
//   5-6  interrupting one waiter does not return another waiter from
//        `wait()`. CratonVM's interrupt wake is a `notify_all` on the
//        monitor's condvar, and every waiter treated a signalled wake as a
//        notification: w2 returned (TERMINATED). With the notification credit
//        on (the default), a signal that leaves no credit now re-parks.
//   7    `Thread.holdsLock(null)` is a NullPointerException with no message
//        (CratonVM's said "Thread.holdsLock(null)").
//
// Not covered, filed instead: HotSpot's wait set is FIFO (`notify()` wakes
// the longest waiter); CratonVM's credit goes to whichever waiter takes the
// state lock first. See
// docs/internal/fixed-bugs/interpreter-L4-notify-does-not-wake-the-longest-waiter-FIXED-20260930.md.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W29TWaitNotify
// (the same lines in every mode; every row is the same wait path, so
// `--compatible` changes too: rows 1-7 were genuine bugs there).
//
// HotSpot 25 (25.0.3, default and -Xint, three runs) prints exactly:
//   1 untimed notify then interrupt: a:returned flag=true
//   2 timed notify then interrupt: a:returned flag=true
//   3 notified, monitor held: BLOCKED / a:returned flag=false
//   4 interrupted, monitor held: BLOCKED / a:IE flag=false
//   5 untimed interrupt w1: w2 WAITING / w1:IE flag=false
//   6 timed interrupt w1: w2 TIMED_WAITING / w1:IE flag=false
//   7 holdsLock(null) java.lang.NullPointerException msg=null

public class L7W29TWaitNotify {
    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long end = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != s && System.nanoTime() < end) Thread.sleep(2);
        Thread.sleep(50);
    }

    static Thread waiter(Object m, String name, StringBuffer log, long timeout) {
        Thread t = new Thread(() -> {
            synchronized (m) {
                try {
                    if (timeout > 0) {
                        m.wait(timeout);
                    } else {
                        m.wait();
                    }
                    log.append(name).append(":returned flag=").append(Thread.currentThread().isInterrupted());
                } catch (InterruptedException e) {
                    log.append(name).append(":IE flag=").append(Thread.currentThread().isInterrupted());
                }
            }
        }, name);
        t.start();
        return t;
    }

    public static void main(String[] args) throws Exception {
        // 1-2. notify() and then interrupt() of the same waiter, both while the
        // notifier holds the monitor: the waiter was notified, so it returns
        // normally with its interrupt pending (untimed and timed wait).
        for (long timeout : new long[] {0, 600_000}) {
            Object m = new Object();
            StringBuffer log = new StringBuffer();
            Thread a = waiter(m, "a", log, timeout);
            awaitState(a, timeout > 0 ? Thread.State.TIMED_WAITING : Thread.State.WAITING);
            synchronized (m) {
                m.notify();
                a.interrupt();
            }
            a.join();
            System.out.println((timeout > 0 ? "2 timed" : "1 untimed") + " notify then interrupt: " + log);
        }
        // 3. a notified waiter re-entering a monitor its notifier still holds is BLOCKED.
        {
            Object m = new Object();
            StringBuffer log = new StringBuffer();
            Thread a = waiter(m, "a", log, 0);
            awaitState(a, Thread.State.WAITING);
            Thread.State s;
            synchronized (m) {
                m.notify();
                Thread.sleep(500);
                s = a.getState();
            }
            a.join();
            System.out.println("3 notified, monitor held: " + s + " / " + log);
        }
        // 4. an interrupted waiter re-entering a held monitor is BLOCKED too.
        {
            Object m = new Object();
            StringBuffer log = new StringBuffer();
            Thread a = waiter(m, "a", log, 0);
            awaitState(a, Thread.State.WAITING);
            Thread.State s;
            synchronized (m) {
                a.interrupt();
                Thread.sleep(500);
                s = a.getState();
            }
            a.join();
            System.out.println("4 interrupted, monitor held: " + s + " / " + log);
        }
        // 5-6. interrupting one waiter does not return the other from wait()
        // (untimed and timed).
        for (long timeout : new long[] {0, 600_000}) {
            Object m = new Object();
            StringBuffer log = new StringBuffer();
            Thread.State waitState = timeout > 0 ? Thread.State.TIMED_WAITING : Thread.State.WAITING;
            Thread w1 = waiter(m, "w1", log, timeout);
            awaitState(w1, waitState);
            Thread w2 = waiter(m, "w2", log, timeout);
            awaitState(w2, waitState);
            w1.interrupt();
            w1.join();
            Thread.sleep(500);
            System.out.println((timeout > 0 ? "6 timed" : "5 untimed") + " interrupt w1: w2 " + w2.getState() + " / " + log);
            synchronized (m) {
                m.notifyAll();
            }
            w2.join();
        }
        // 7. Thread.holdsLock(null): NullPointerException with no message.
        try {
            System.out.println("7 holdsLock(null) returned " + Thread.holdsLock(null));
        } catch (NullPointerException e) {
            System.out.println("7 holdsLock(null) " + e.getClass().getName() + " msg=" + e.getMessage());
        }
    }
}
