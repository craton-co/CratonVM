// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L7: `Thread.getState()` of a thread
// blocked in each JDK blocking primitive, for a platform thread and a virtual
// thread. Each row starts the thread, polls its state (up to 5 s) until it
// leaves NEW / RUNNABLE, prints what it saw, then wakes the thread and joins
// it.
//
// Pages: `interpreter-L0-a-parked-virtual-thread-reports-runnable` (every
// virtual-thread row answered RUNNABLE: the continuation yield set no thread
// state) and, found while writing this probe, the timed rows (CratonVM stored
// WAITING for timed parks, waits and joins).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L7W23ThreadStates
//
// HotSpot 25 prints exactly:
//     platform new NEW
//     platform park WAITING
//     platform latch WAITING
//     platform parkNanos TIMED_WAITING
//     platform sleep TIMED_WAITING
//     platform wait WAITING
//     platform waitMs TIMED_WAITING
//     platform join WAITING
//     platform joinMs TIMED_WAITING
//     platform monitor BLOCKED
//     platform terminated TERMINATED
//     virtual new NEW
//     virtual park WAITING
//     virtual latch WAITING
//     virtual parkNanos TIMED_WAITING
//     virtual sleep TIMED_WAITING
//     virtual wait WAITING
//     virtual waitMs TIMED_WAITING
//     virtual join WAITING
//     virtual joinMs TIMED_WAITING
//     virtual monitor BLOCKED
//     virtual terminated TERMINATED
//
// CratonVM before wave 23 (read from the code, not run): every `virtual` row
// that unmounts (park, latch, parkNanos, sleep) RUNNABLE after the 5 s poll;
// `parkNanos`, `waitMs` and `joinMs` WAITING (the timed park, timed
// `Object.wait` and the `Thread.join(long)` native stored the untimed state).
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L7W23ThreadStates {
    static final long LONG_NANOS = 20_000_000_000L;
    static final long LONG_MS = 20_000L;

    interface Body {
        void run() throws Exception;
    }

    static Thread make(boolean virtual, Body body) {
        Runnable r = () -> {
            try {
                body.run();
            } catch (Throwable t) {
                // woken by interrupt, or done
            }
        };
        return virtual ? Thread.ofVirtual().unstarted(r) : Thread.ofPlatform().unstarted(r);
    }

    static Thread.State settle(Thread t) throws InterruptedException {
        long end = System.nanoTime() + 5_000_000_000L;
        Thread.State s = t.getState();
        while ((s == Thread.State.NEW || s == Thread.State.RUNNABLE) && System.nanoTime() < end) {
            Thread.sleep(5);
            s = t.getState();
        }
        return s;
    }

    static void row(String kind, String name, Thread t, Runnable wake) throws InterruptedException {
        t.start();
        Thread.State s = settle(t);
        System.out.println(kind + " " + name + " " + s);
        wake.run();
        t.join(30_000);
    }

    static void run(boolean virtual) throws Exception {
        String kind = virtual ? "virtual" : "platform";
        System.out.println(kind + " new " + make(virtual, () -> { }).getState());

        Thread[] self = new Thread[1];
        self[0] = make(virtual, () -> LockSupport.park());
        Thread parked = self[0];
        row(kind, "park", parked, () -> LockSupport.unpark(parked));

        CountDownLatch latch = new CountDownLatch(1);
        row(kind, "latch", make(virtual, latch::await), latch::countDown);

        Thread pn = make(virtual, () -> LockSupport.parkNanos(LONG_NANOS));
        row(kind, "parkNanos", pn, () -> LockSupport.unpark(pn));

        // 3 s, not LONG_MS: the row wakes it by interrupt, which does not yet
        // reach an unmounted virtual thread on CratonVM (open page
        // `i23-L7-an-interrupt-does-not-reach-an-unmounted-virtual-thread`);
        // the state is read long before either wake.
        Thread sl = make(virtual, () -> Thread.sleep(3_000));
        row(kind, "sleep", sl, sl::interrupt);

        Object lock = new Object();
        Thread w = make(virtual, () -> {
            synchronized (lock) {
                lock.wait();
            }
        });
        row(kind, "wait", w, () -> {
            synchronized (lock) {
                lock.notifyAll();
            }
        });

        Thread wm = make(virtual, () -> {
            synchronized (lock) {
                lock.wait(LONG_MS);
            }
        });
        row(kind, "waitMs", wm, () -> {
            synchronized (lock) {
                lock.notifyAll();
            }
        });

        CountDownLatch release1 = new CountDownLatch(1);
        Thread target1 = make(false, release1::await);
        target1.start();
        row(kind, "join", make(virtual, target1::join), release1::countDown);
        target1.join();
        CountDownLatch release2 = new CountDownLatch(1);
        Thread target2 = make(false, release2::await);
        target2.start();
        row(kind, "joinMs", make(virtual, () -> target2.join(LONG_MS)), release2::countDown);
        target2.join();

        Object mon = new Object();
        CountDownLatch held = new CountDownLatch(1);
        CountDownLatch unlock = new CountDownLatch(1);
        Thread owner = new Thread(() -> {
            synchronized (mon) {
                held.countDown();
                try {
                    unlock.await();
                } catch (InterruptedException e) {
                }
            }
        });
        owner.start();
        held.await();
        row(kind, "monitor", make(virtual, () -> {
            synchronized (mon) {
                mon.hashCode();
            }
        }), unlock::countDown);
        owner.join();

        Thread done = make(virtual, () -> { });
        done.start();
        done.join();
        System.out.println(kind + " terminated " + done.getState());
    }

    public static void main(String[] args) throws Exception {
        run(false);
        run(true);
    }
}
