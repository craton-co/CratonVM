// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29 (orchestrator): `notify()` wakes the thread
// that has waited longest, as HotSpot's `ObjectMonitor` wait set does
// (docs/known-issues/interpreter/i29-L4-notify-does-not-wake-the-longest-waiter-20260930.md).
//
// Three waiters enter `wait()` on one lock in a known order (each is seen
// WAITING before the next starts), then `notify()` runs three times, each in
// its own `synchronized` block, and after each the probe prints which waiters
// have returned. Then a notified-and-interrupted row: the longest waiter is
// notified and interrupted inside one `synchronized` block; it returns
// normally with its interrupt pending, and the other waiter stays WAITING.
//
// Before wave 29 the notification was an anonymous credit that any waiter
// could take, so the rows were a race.
//
// Run: javac -d out L7W29TNotifyOrder.java && cratonvm --java-home <jdk25> [--nojit] -cp out L7W29TNotifyOrder
//
// Expected HotSpot 25 output (default and -Xint):
//   notify 1: w1
//   notify 2: w1 w2
//   notify 3: w1 w2 w3
//   notify+interrupt: v1=returned interrupted=true v2=WAITING
public class L7W29TNotifyOrder {
    static final Object LOCK = new Object();

    static final class Waiter extends Thread {
        volatile String result = "";
        volatile boolean interruptedAfter;

        Waiter(String name) {
            super(name);
            setDaemon(true);
        }

        @Override
        public void run() {
            synchronized (LOCK) {
                try {
                    LOCK.wait();
                    result = "returned";
                    interruptedAfter = Thread.currentThread().isInterrupted();
                } catch (InterruptedException e) {
                    result = "interrupted";
                }
            }
        }
    }

    static void awaitWaiting(Thread t) throws InterruptedException {
        while (t.getState() != Thread.State.WAITING) {
            Thread.sleep(5);
        }
    }

    static void awaitDone(Thread t) throws InterruptedException {
        t.join(5000);
    }

    public static void main(String[] args) throws Exception {
        Waiter[] ws = { new Waiter("w1"), new Waiter("w2"), new Waiter("w3") };
        for (Waiter w : ws) {
            w.start();
            awaitWaiting(w);
        }
        for (int i = 0; i < 3; i++) {
            synchronized (LOCK) {
                LOCK.notify();
            }
            awaitDone(ws[i]);
            // Give a wrongly woken waiter time to show itself.
            Thread.sleep(100);
            StringBuilder sb = new StringBuilder("notify " + (i + 1) + ":");
            for (Waiter w : ws) {
                if (!w.isAlive()) {
                    sb.append(' ').append(w.getName());
                }
            }
            System.out.println(sb);
        }

        Waiter v1 = new Waiter("v1");
        Waiter v2 = new Waiter("v2");
        v1.start();
        awaitWaiting(v1);
        v2.start();
        awaitWaiting(v2);
        synchronized (LOCK) {
            LOCK.notify();
            v1.interrupt();
        }
        awaitDone(v1);
        Thread.sleep(100);
        System.out.println("notify+interrupt: v1=" + v1.result + " interrupted=" + v1.interruptedAfter
                + " v2=" + v2.getState());
        synchronized (LOCK) {
            LOCK.notifyAll();
        }
        awaitDone(v2);
    }
}
