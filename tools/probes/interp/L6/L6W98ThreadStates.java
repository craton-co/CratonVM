// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L6 -- Thread.getState() transitions of a
// platform and a virtual thread against HotSpot 25: NEW, RUNNABLE, BLOCKED on
// a monitor, WAITING in Object.wait / LockSupport.park / Thread.join,
// TIMED_WAITING in sleep / wait(ms) / parkNanos / join(ms), BLOCKED again
// after a notify while the notifier still holds the monitor, TERMINATED. Each
// state is polled for (up to 5 s) and the line prints the first matching
// answer, or "never-<last>" when it did not come. No timing in the output.
//
// HOTSPOT_EXPECTED (JDK 25, both kinds): every line "<kind> <case> <STATE>"
// with STATE the expected one, i.e. no "never-" line.
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L6W98ThreadStates {
    static final Object LOCK = new Object();
    static volatile boolean stop;

    static String await(Thread t, Thread.State want) throws InterruptedException {
        Thread.State last = t.getState();
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (last != want && System.nanoTime() < deadline) {
            Thread.sleep(1);
            last = t.getState();
        }
        return last == want ? last.toString() : "never-" + last;
    }

    static Thread make(boolean virtual, Runnable r) {
        return virtual ? Thread.ofVirtual().unstarted(r) : Thread.ofPlatform().unstarted(r);
    }

    static void run(boolean virtual) throws Exception {
        String kind = virtual ? "virtual" : "platform";

        // NEW, RUNNABLE (spinning), TERMINATED
        stop = false;
        Thread spin = make(virtual, () -> {
            while (!stop) {
                Thread.onSpinWait();
            }
        });
        System.out.println(kind + " new " + spin.getState());
        spin.start();
        System.out.println(kind + " running " + await(spin, Thread.State.RUNNABLE));
        stop = true;
        spin.join();
        System.out.println(kind + " terminated " + spin.getState());

        // BLOCKED entering a held monitor
        Thread blocked;
        synchronized (LOCK) {
            blocked = make(virtual, () -> {
                synchronized (LOCK) {
                    LOCK.hashCode();
                }
            });
            blocked.start();
            System.out.println(kind + " monitor-enter " + await(blocked, Thread.State.BLOCKED));
        }
        blocked.join();

        // WAITING in Object.wait(), then BLOCKED after notify while held
        CountDownLatch inWait = new CountDownLatch(1);
        Thread waiter = make(virtual, () -> {
            synchronized (LOCK) {
                inWait.countDown();
                try {
                    LOCK.wait();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
        });
        waiter.start();
        inWait.await();
        System.out.println(kind + " object-wait " + await(waiter, Thread.State.WAITING));
        synchronized (LOCK) {
            LOCK.notifyAll();
            System.out.println(kind + " notified-reacquire " + await(waiter, Thread.State.BLOCKED));
        }
        waiter.join();

        // TIMED_WAITING in Object.wait(ms)
        CountDownLatch inTimed = new CountDownLatch(1);
        Thread timedWaiter = make(virtual, () -> {
            synchronized (LOCK) {
                inTimed.countDown();
                try {
                    LOCK.wait(60_000);
                } catch (InterruptedException e) {
                    // expected
                }
            }
        });
        timedWaiter.start();
        inTimed.await();
        System.out.println(kind + " object-wait-ms " + await(timedWaiter, Thread.State.TIMED_WAITING));
        timedWaiter.interrupt();
        timedWaiter.join();

        // WAITING in park, TIMED_WAITING in parkNanos
        Thread parker = make(virtual, LockSupport::park);
        parker.start();
        System.out.println(kind + " park " + await(parker, Thread.State.WAITING));
        LockSupport.unpark(parker);
        parker.join();
        stop = false;
        Thread nanoParker = make(virtual, () -> {
            while (!stop) {
                LockSupport.parkNanos(30_000_000_000L);
            }
        });
        nanoParker.start();
        System.out.println(kind + " park-nanos " + await(nanoParker, Thread.State.TIMED_WAITING));
        stop = true;
        LockSupport.unpark(nanoParker);
        nanoParker.join();

        // TIMED_WAITING in sleep
        Thread sleeper = make(virtual, () -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                // expected
            }
        });
        sleeper.start();
        System.out.println(kind + " sleep " + await(sleeper, Thread.State.TIMED_WAITING));
        sleeper.interrupt();
        sleeper.join();

        // WAITING in join(), TIMED_WAITING in join(ms)
        Thread target = make(virtual, LockSupport::park);
        target.start();
        await(target, Thread.State.WAITING);
        Thread joiner = make(virtual, () -> {
            try {
                target.join();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        joiner.start();
        System.out.println(kind + " join " + await(joiner, Thread.State.WAITING));
        Thread timedJoiner = make(virtual, () -> {
            try {
                target.join(60_000);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        });
        timedJoiner.start();
        System.out.println(kind + " join-ms " + await(timedJoiner, Thread.State.TIMED_WAITING));
        LockSupport.unpark(target);
        target.join();
        joiner.join();
        timedJoiner.join();
        System.out.println(kind + " joiners-done " + joiner.getState() + " " + timedJoiner.getState());
    }

    public static void main(String[] args) throws Exception {
        run(false);
        run(true);
        System.out.println("done");
    }
}
