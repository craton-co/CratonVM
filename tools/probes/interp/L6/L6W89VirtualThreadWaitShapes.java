// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 89, lane L6 -- Object.wait semantics of a virtual
// thread, the shapes the unmounted wait (`CRATONVM_VTHREAD_MONITOR_UNMOUNT`)
// must keep. Expected output is HotSpot 25's (JEP 491 unmounts all of these;
// derived from java/lang/{Object,VirtualThread}.java and ObjectMonitor::wait):
// * notified -> returns normally; an interrupt that came too stays pending;
// * interrupted (not notified) -> InterruptedException, no message, status
//   cleared;
// * timed out -> returns normally;
// * the recursion count of nested `synchronized` is back after the wait;
// * Thread.getState() while waiting is WAITING / TIMED_WAITING;
// * a virtual thread joining a platform thread (Thread.join is a wait on the
//   Thread object) wakes when that thread ends;
// * notify() wakes one waiter, notifyAll() the rest.
//
// Run, each mode, with the switch off and on (the output must not change):
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W89VirtualThreadWaitShapes
//   CRATONVM_VTHREAD_MONITOR_UNMOUNT=1 cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W89VirtualThreadWaitShapes
//   (and both with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN
// notified: returned interrupted-after=true
// interrupted: InterruptedException null interrupted-after=false
// timed out: returned
// recursion after wait: held=true exits=3 heldAfter=false
// states while waiting: WAITING TIMED_WAITING
// join of a platform thread: joined=true
// notify wakes one: 1 then notifyAll: 3
// HOTSPOT_EXPECTED_END

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public class L6W89VirtualThreadWaitShapes {
    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (t.getState() != s && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (System.getProperty("jdk.virtualThreadScheduler.parallelism") == null) {
            System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        }

        // Notified, with an interrupt arriving after the notify.
        Object a = new Object();
        AtomicReference<String> out = new AtomicReference<>();
        Thread t1 = Thread.ofVirtual().start(() -> {
            synchronized (a) {
                try {
                    a.wait();
                    out.set("returned interrupted-after=" + Thread.currentThread().isInterrupted());
                } catch (InterruptedException e) {
                    out.set("InterruptedException");
                }
            }
        });
        awaitState(t1, Thread.State.WAITING);
        synchronized (a) {
            a.notify();
            t1.interrupt();
        }
        t1.join();
        System.out.println("notified: " + out.get());

        // Interrupted, not notified.
        Object b = new Object();
        Thread t2 = Thread.ofVirtual().start(() -> {
            synchronized (b) {
                try {
                    b.wait();
                    out.set("returned");
                } catch (InterruptedException e) {
                    out.set("InterruptedException " + e.getMessage()
                            + " interrupted-after=" + Thread.currentThread().isInterrupted());
                }
            }
        });
        awaitState(t2, Thread.State.WAITING);
        t2.interrupt();
        t2.join();
        System.out.println("interrupted: " + out.get());

        // Timed out.
        Object c = new Object();
        Thread t3 = Thread.ofVirtual().start(() -> {
            synchronized (c) {
                try {
                    c.wait(30);
                    out.set("returned");
                } catch (InterruptedException e) {
                    out.set("InterruptedException");
                }
            }
        });
        t3.join();
        System.out.println("timed out: " + out.get());

        // Recursion: three nested holds, wait, then the holds are all back.
        Object d = new Object();
        Thread t4 = Thread.ofVirtual().start(() -> {
            synchronized (d) {
                synchronized (d) {
                    synchronized (d) {
                        try {
                            d.wait(20);
                        } catch (InterruptedException e) {
                            throw new RuntimeException(e);
                        }
                        out.set("held=" + Thread.holdsLock(d));
                    }
                }
            }
            out.set(out.get() + " exits=3 heldAfter=" + Thread.holdsLock(d));
        });
        t4.join();
        System.out.println("recursion after wait: " + out.get());

        // States while waiting.
        Object e = new Object();
        Thread w1 = Thread.ofVirtual().start(() -> {
            synchronized (e) {
                try {
                    e.wait();
                } catch (InterruptedException ex) {
                    throw new RuntimeException(ex);
                }
            }
        });
        Thread w2 = Thread.ofVirtual().start(() -> {
            synchronized (e) {
                try {
                    e.wait(60_000);
                } catch (InterruptedException ex) {
                    throw new RuntimeException(ex);
                }
            }
        });
        awaitState(w1, Thread.State.WAITING);
        awaitState(w2, Thread.State.TIMED_WAITING);
        System.out.println("states while waiting: " + w1.getState() + " " + w2.getState());
        synchronized (e) {
            e.notifyAll();
        }
        w1.join();
        w2.join();

        // A virtual thread joining a platform thread.
        CountDownLatch go = new CountDownLatch(1);
        Thread platform = Thread.ofPlatform().start(() -> {
            try {
                go.await();
            } catch (InterruptedException ex) {
                throw new RuntimeException(ex);
            }
        });
        AtomicReference<Boolean> joined = new AtomicReference<>(false);
        Thread joiner = Thread.ofVirtual().start(() -> {
            try {
                platform.join();
                joined.set(!platform.isAlive());
            } catch (InterruptedException ex) {
                throw new RuntimeException(ex);
            }
        });
        awaitState(joiner, Thread.State.WAITING);
        go.countDown();
        joiner.join(TimeUnit.SECONDS.toMillis(10));
        System.out.println("join of a platform thread: joined=" + joined.get());

        // notify wakes one, notifyAll the rest.
        Object f = new Object();
        AtomicInteger woke = new AtomicInteger();
        Thread[] ws = new Thread[4];
        for (int i = 0; i < ws.length; i++) {
            ws[i] = Thread.ofVirtual().start(() -> {
                synchronized (f) {
                    try {
                        f.wait();
                    } catch (InterruptedException ex) {
                        throw new RuntimeException(ex);
                    }
                    woke.incrementAndGet();
                }
            });
        }
        for (Thread w : ws) {
            awaitState(w, Thread.State.WAITING);
        }
        synchronized (f) {
            f.notify();
        }
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (woke.get() < 1 && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(100);
        int afterOne = woke.get();
        synchronized (f) {
            f.notifyAll();
        }
        for (Thread w : ws) {
            w.join(TimeUnit.SECONDS.toMillis(10));
        }
        System.out.println("notify wakes one: " + afterOne + " then notifyAll: " + (woke.get() - afterOne));
        System.exit(0);
    }
}
