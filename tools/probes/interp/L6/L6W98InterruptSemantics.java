// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L6 -- interrupt semantics and the
// exception texts of the blocking Thread / Object calls, on a platform and a
// virtual thread, against HotSpot 25. For each blocking call entered with the
// interrupt status already set: does it throw, the exception's class and
// getMessage(), and the interrupt status after. Also the argument checks
// (negative timeouts, nanos out of range), IllegalMonitorStateException of
// wait/notify/notifyAll without the monitor, Thread.holdsLock, interrupt of an
// unstarted / terminated thread, Thread.interrupted() clearing, and a blocked
// wait/sleep/park woken by interrupt(). No timing in the output.
import java.time.Duration;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

public class L6W98InterruptSemantics {
    interface Body {
        void run() throws Exception;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void pending(String kind, String what, Body b) {
        Thread.currentThread().interrupt();
        String o = outcome(b);
        boolean after = Thread.interrupted();
        System.out.println(kind + " pending " + what + " -> " + o + " | status-after=" + after);
    }

    static void body(String kind) throws Exception {
        Object lock = new Object();
        Thread done = new Thread(() -> { });
        done.start();
        done.join();
        Thread parked = Thread.ofPlatform().daemon().start(LockSupport::park);

        pending(kind, "sleep(10)", () -> Thread.sleep(10));
        pending(kind, "sleep(0)", () -> Thread.sleep(0));
        pending(kind, "sleep(Duration)", () -> Thread.sleep(Duration.ofMillis(10)));
        pending(kind, "wait(10)", () -> {
            synchronized (lock) {
                lock.wait(10);
            }
        });
        pending(kind, "wait()", () -> {
            synchronized (lock) {
                lock.wait();
            }
        });
        pending(kind, "join-alive(10)", () -> parked.join(10));
        pending(kind, "join-dead()", done::join);
        pending(kind, "join(Duration)-alive", () -> System.out.print(parked.join(Duration.ofMillis(10)) + " "));
        pending(kind, "park()", LockSupport::park);
        pending(kind, "parkNanos(10ms)", () -> LockSupport.parkNanos(10_000_000L));
        pending(kind, "yield", Thread::yield);

        System.out.println(kind + " sleep(-1) -> " + outcome(() -> Thread.sleep(-1)));
        System.out.println(kind + " sleep(0,-1) -> " + outcome(() -> Thread.sleep(0, -1)));
        System.out.println(kind + " sleep(0,1000000) -> " + outcome(() -> Thread.sleep(0, 1_000_000)));
        System.out.println(kind + " wait(-1) -> " + outcome(() -> {
            synchronized (lock) {
                lock.wait(-1);
            }
        }));
        System.out.println(kind + " wait(0,1000000) -> " + outcome(() -> {
            synchronized (lock) {
                lock.wait(0, 1_000_000);
            }
        }));
        System.out.println(kind + " join(-1) -> " + outcome(() -> done.join(-1)));
        System.out.println(kind + " join(0,-1) -> " + outcome(() -> done.join(0, -1)));
        System.out.println(kind + " wait-unowned -> " + outcome(() -> lock.wait()));
        System.out.println(kind + " wait-ms-unowned -> " + outcome(() -> lock.wait(5)));
        System.out.println(kind + " notify-unowned -> " + outcome(lock::notify));
        System.out.println(kind + " notifyAll-unowned -> " + outcome(lock::notifyAll));
        System.out.println(kind + " holdsLock(null) -> " + outcome(() -> Thread.holdsLock(null)));
        boolean outside = Thread.holdsLock(lock);
        boolean inside;
        synchronized (lock) {
            inside = Thread.holdsLock(lock);
        }
        System.out.println(kind + " holdsLock outside=" + outside + " inside=" + inside);

        // Thread.interrupted() clears, isInterrupted() does not.
        Thread.currentThread().interrupt();
        boolean is1 = Thread.currentThread().isInterrupted();
        boolean i1 = Thread.interrupted();
        boolean i2 = Thread.interrupted();
        System.out.println(kind + " isInterrupted=" + is1 + " interrupted=" + i1 + " then=" + i2);

        // Woken by interrupt() while blocked.
        CountDownLatch entered = new CountDownLatch(1);
        Thread self = Thread.currentThread();
        Thread interrupter = Thread.ofPlatform().start(() -> {
            try {
                entered.await();
                while (self.getState() == Thread.State.RUNNABLE) {
                    Thread.sleep(1);
                }
            } catch (InterruptedException e) {
                return;
            }
            self.interrupt();
        });
        entered.countDown();
        String woke = outcome(() -> Thread.sleep(60_000));
        System.out.println(kind + " blocked sleep woken -> " + woke + " | status-after=" + Thread.interrupted());
        interrupter.join();

        CountDownLatch entered2 = new CountDownLatch(1);
        Thread interrupter2 = Thread.ofPlatform().start(() -> {
            try {
                entered2.await();
                synchronized (lock) {
                    // the waiter is inside wait() once it released the lock
                }
                while (self.getState() != Thread.State.WAITING) {
                    Thread.sleep(1);
                }
            } catch (InterruptedException e) {
                return;
            }
            self.interrupt();
        });
        String woke2;
        synchronized (lock) {
            entered2.countDown();
            woke2 = outcome(() -> lock.wait());
        }
        System.out.println(kind + " blocked wait woken -> " + woke2 + " | status-after=" + Thread.interrupted());
        interrupter2.join();

        Thread interrupter3 = Thread.ofPlatform().start(() -> {
            try {
                while (self.getState() != Thread.State.WAITING) {
                    Thread.sleep(1);
                }
            } catch (InterruptedException e) {
                return;
            }
            self.interrupt();
        });
        LockSupport.park();
        // a spurious return is allowed, so loop until the interrupt is seen
        while (!Thread.currentThread().isInterrupted()) {
            LockSupport.park();
        }
        System.out.println(kind + " blocked park woken | status-after=" + Thread.interrupted());
        interrupter3.join();

        LockSupport.unpark(parked);
        parked.join();
    }

    public static void main(String[] args) throws Exception {
        body("platform");
        Thread v = Thread.ofVirtual().start(() -> {
            try {
                body("virtual");
            } catch (Exception e) {
                e.printStackTrace(System.out);
            }
        });
        v.join();

        // interrupt of an unstarted and of a terminated thread
        Thread t = new Thread(() -> System.out.println("unstarted-interrupted run sees " + Thread.currentThread().isInterrupted()));
        t.interrupt();
        System.out.println("unstarted platform isInterrupted=" + t.isInterrupted());
        t.start();
        t.join();
        System.out.println("terminated platform isInterrupted=" + t.isInterrupted());
        t.interrupt();
        System.out.println("terminated platform after interrupt isInterrupted=" + t.isInterrupted());
        Thread u = Thread.ofVirtual().unstarted(() -> System.out.println("unstarted-interrupted vthread sees " + Thread.currentThread().isInterrupted()));
        u.interrupt();
        System.out.println("unstarted virtual isInterrupted=" + u.isInterrupted());
        u.start();
        u.join();
        System.out.println("terminated virtual isInterrupted=" + u.isInterrupted());
        System.out.println("done");
    }
}
