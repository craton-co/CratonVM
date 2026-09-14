// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.Exchanger;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;

/**
 * The blocking half of {@code java.util.concurrent}, exercised while the
 * collector is running underneath it.
 *
 * <p>WHY THIS EXISTS. Every one of these classes is implemented by a native in
 * this VM, and each of those natives holds a Java {@code ObjectRef} across a
 * call that can PARK or RE-ENTER JAVA -- a monitor wait, a
 * {@code monitor_enter_gc_safe}, an {@code invoke_virtual} into user code. A
 * collection can run inside any of those windows, so every such reference has to
 * be pinned and re-read afterwards; a native that skips the re-read exits a
 * monitor at a pre-GC address, which is a fault when the young slot was
 * reclaimed and a PERMANENTLY LEAKED LOCK when it was merely moved. That is the
 * defect this file was written to catch, in
 * {@code Collections$Synchronized*}'s {@code with_sync_mutex}.
 *
 * <p>EVERY ROW IS AN EXACT COUNT, never a timing. A leaked monitor shows up as a
 * hang (caught by the per-phase deadline), a stale receiver as a fault or as a
 * lost element; an interleaving shows up as nothing at all. Run it under
 * {@code --XX:UseGc Generational}, with and without {@code --nojit}, and with
 * {@code CRATONVM_DBG_GC_STRESS} set so a collection lands inside the blocking
 * windows rather than between them.
 */
public class ConcurrencyUnderGcSweep {

    static final int THREADS = Integer.getInteger("cugs.threads", 6);
    static final int ITERS = Integer.getInteger("cugs.iters", 300);
    static final int CHURN = Integer.getInteger("cugs.churn", 128);
    static final long PHASE_TIMEOUT_SEC = Integer.getInteger("cugs.timeout", 120);

    static final List<String> failures = new CopyOnWriteArrayList<>();
    static volatile Object sink;

    static void fail(String s) {
        failures.add(s);
    }

    static void eq(String tag, long got, long want) {
        if (got != want) {
            fail(tag + ": " + got + " != " + want);
        }
    }

    /** Allocate garbage so a collection lands inside the blocking windows. */
    static void churn() {
        Object last = null;
        for (int i = 0; i < CHURN; i++) {
            last = new byte[48];
        }
        sink = last;
    }

    /** A payload whose identity is checkable after it has travelled through a queue. */
    static final class Item implements Comparable<Item> {
        final int id;
        final String tag;

        Item(int id) {
            this.id = id;
            this.tag = "i" + id;
        }

        @Override
        public int compareTo(Item o) {
            return Integer.compare(id, o.id);
        }

        boolean intact() {
            return System.identityHashCode(this) != 0 && ("i" + id).equals(tag);
        }
    }

    interface Phase {
        void run() throws Throwable;
    }

    static void phase(String name, Phase p) {
        long t0 = System.nanoTime();
        Thread runner = new Thread(() -> {
            try {
                p.run();
            } catch (Throwable e) {
                fail(name + " threw " + e);
            }
        }, "cugs-" + name);
        runner.setDaemon(true);
        runner.start();
        try {
            runner.join(PHASE_TIMEOUT_SEC * 1000L);
        } catch (InterruptedException e) {
            fail(name + " interrupted");
        }
        if (runner.isAlive()) {
            fail(name + " DID NOT FINISH in " + PHASE_TIMEOUT_SEC + "s (leaked monitor / lost wakeup)");
        }
        System.out.println("  phase " + name + " " + ((System.nanoTime() - t0) / 1_000_000L) + "ms");
    }

    static Thread[] fork(String name, Runnable body) {
        Thread[] ts = new Thread[THREADS];
        for (int i = 0; i < THREADS; i++) {
            ts[i] = new Thread(body, name + "-" + i);
            ts[i].setDaemon(true);
            ts[i].start();
        }
        return ts;
    }

    static void join(Thread[] ts) throws InterruptedException {
        for (Thread t : ts) {
            t.join();
        }
    }

    public static void main(String[] args) throws Exception {
        phase("semaphore", () -> {
            Semaphore sem = new Semaphore(2);
            AtomicLong inside = new AtomicLong();
            AtomicLong maxInside = new AtomicLong();
            Thread[] ts = fork("sem", () -> {
                try {
                    for (int i = 0; i < ITERS; i++) {
                        sem.acquire();
                        long n = inside.incrementAndGet();
                        maxInside.accumulateAndGet(n, Math::max);
                        churn();
                        inside.decrementAndGet();
                        sem.release();
                    }
                } catch (Throwable e) {
                    fail("semaphore body " + e);
                }
            });
            join(ts);
            eq("semaphore permits restored", sem.availablePermits(), 2);
            if (maxInside.get() > 2) {
                fail("semaphore let " + maxInside.get() + " threads in, limit was 2");
            }
        });

        phase("countdownlatch", () -> {
            int n = THREADS * 20;
            CountDownLatch latch = new CountDownLatch(n);
            AtomicLong counted = new AtomicLong();
            Thread[] ts = fork("cdl", () -> {
                for (int i = 0; i < 20; i++) {
                    churn();
                    counted.incrementAndGet();
                    latch.countDown();
                }
            });
            if (!latch.await(PHASE_TIMEOUT_SEC, TimeUnit.SECONDS)) {
                fail("countdownlatch await timed out at count=" + latch.getCount());
            }
            join(ts);
            eq("countdownlatch counted", counted.get(), n);
            eq("countdownlatch remaining", latch.getCount(), 0);
        });

        phase("cyclicbarrier", () -> {
            AtomicLong trips = new AtomicLong();
            int rounds = 40;
            CyclicBarrier barrier = new CyclicBarrier(THREADS, trips::incrementAndGet);
            Thread[] ts = fork("cb", () -> {
                try {
                    for (int i = 0; i < rounds; i++) {
                        churn();
                        barrier.await();
                    }
                } catch (Throwable e) {
                    fail("cyclicbarrier body " + e);
                }
            });
            join(ts);
            eq("cyclicbarrier trips", trips.get(), rounds);
        });

        phase("exchanger", () -> {
            Exchanger<Item> x = new Exchanger<>();
            int rounds = 60;
            AtomicLong exchanged = new AtomicLong();
            Runnable side = () -> {
                try {
                    for (int i = 0; i < rounds; i++) {
                        Item mine = new Item(Thread.currentThread().getName().hashCode() ^ i);
                        churn();
                        Item theirs = x.exchange(mine, PHASE_TIMEOUT_SEC, TimeUnit.SECONDS);
                        if (theirs == null || !theirs.intact()) {
                            fail("exchanger got a damaged payload at round " + i);
                        }
                        exchanged.incrementAndGet();
                    }
                } catch (Throwable e) {
                    fail("exchanger body " + e);
                }
            };
            Thread a = new Thread(side, "xch-a");
            Thread b = new Thread(side, "xch-b");
            a.setDaemon(true);
            b.setDaemon(true);
            a.start();
            b.start();
            a.join();
            b.join();
            eq("exchanger rounds", exchanged.get(), rounds * 2L);
        });

        queuePhase("linkedblockingqueue", new LinkedBlockingQueue<>());
        queuePhase("arrayblockingqueue", new ArrayBlockingQueue<>(64));
        queuePhase("priorityblockingqueue", new PriorityBlockingQueue<>());

        phase("reentrantlock+condition", () -> {
            ReentrantLock lock = new ReentrantLock();
            Condition notEmpty = lock.newCondition();
            List<Item> box = new ArrayList<>();
            int total = THREADS * 50;
            AtomicLong consumed = new AtomicLong();
            AtomicLong sum = new AtomicLong();
            Thread consumer = new Thread(() -> {
                try {
                    while (consumed.get() < total) {
                        lock.lock();
                        try {
                            while (box.isEmpty()) {
                                if (!notEmpty.await(PHASE_TIMEOUT_SEC, TimeUnit.SECONDS)) {
                                    fail("condition await timed out");
                                    return;
                                }
                            }
                            Item it = box.remove(box.size() - 1);
                            if (!it.intact()) {
                                fail("condition consumer got a damaged item");
                            }
                            sum.addAndGet(it.id);
                            consumed.incrementAndGet();
                        } finally {
                            lock.unlock();
                        }
                        churn();
                    }
                } catch (Throwable e) {
                    fail("condition consumer " + e);
                }
            }, "cond-consumer");
            consumer.setDaemon(true);
            consumer.start();
            long[] produced = new long[1];
            Thread[] ts = fork("cond-prod", () -> {
                for (int i = 0; i < 50; i++) {
                    Item it = new Item(i + 1);
                    lock.lock();
                    try {
                        box.add(it);
                        synchronized (produced) {
                            produced[0] += it.id;
                        }
                        notEmpty.signalAll();
                    } finally {
                        lock.unlock();
                    }
                    churn();
                }
            });
            join(ts);
            consumer.join(PHASE_TIMEOUT_SEC * 1000L);
            eq("condition consumed", consumed.get(), total);
            eq("condition sum", sum.get(), produced[0]);
        });

        phase("readwritelock", () -> {
            ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
            final Item[] cell = { new Item(0) };
            AtomicLong writes = new AtomicLong();
            Thread[] ts = fork("rw", () -> {
                for (int i = 0; i < ITERS; i++) {
                    if ((i & 7) == 0) {
                        rw.writeLock().lock();
                        try {
                            cell[0] = new Item(i);
                            writes.incrementAndGet();
                        } finally {
                            rw.writeLock().unlock();
                        }
                    } else {
                        rw.readLock().lock();
                        try {
                            if (!cell[0].intact()) {
                                fail("readwritelock read a damaged cell");
                            }
                        } finally {
                            rw.readLock().unlock();
                        }
                    }
                    churn();
                }
            });
            join(ts);
            if (rw.isWriteLocked()) {
                fail("readwritelock still write-locked after every writer returned");
            }
            eq("readwritelock writes", writes.get(), THREADS * ((ITERS + 7) / 8L));
        });

        phase("synchronized+wait/notify", () -> {
            final Object mon = new Object();
            final List<Item> box = new ArrayList<>();
            int perThread = 40;
            int total = THREADS * perThread;
            AtomicLong drained = new AtomicLong();
            Thread drainer = new Thread(() -> {
                try {
                    while (drained.get() < total) {
                        synchronized (mon) {
                            while (box.isEmpty()) {
                                mon.wait(PHASE_TIMEOUT_SEC * 1000L);
                                if (box.isEmpty()) {
                                    fail("wait/notify: woke with an empty box (lost wakeup)");
                                    return;
                                }
                            }
                            Item it = box.remove(box.size() - 1);
                            if (!it.intact()) {
                                fail("wait/notify drained a damaged item");
                            }
                            drained.incrementAndGet();
                        }
                        churn();
                    }
                } catch (Throwable e) {
                    fail("wait/notify drainer " + e);
                }
            }, "wn-drainer");
            drainer.setDaemon(true);
            drainer.start();
            Thread[] ts = fork("wn-prod", () -> {
                for (int i = 0; i < perThread; i++) {
                    synchronized (mon) {
                        box.add(new Item(i));
                        mon.notifyAll();
                    }
                    churn();
                }
            });
            join(ts);
            drainer.join(PHASE_TIMEOUT_SEC * 1000L);
            eq("wait/notify drained", drained.get(), total);
        });

        phase("copyonwritearraylist", () -> {
            CopyOnWriteArrayList<Item> cow = new CopyOnWriteArrayList<>();
            int perThread = 100;
            Thread[] ts = fork("cow", () -> {
                for (int i = 0; i < perThread; i++) {
                    cow.add(new Item(i));
                    churn();
                    for (Item it : cow) {
                        if (!it.intact()) {
                            fail("copyonwritearraylist iterated a damaged item");
                            return;
                        }
                    }
                }
            });
            join(ts);
            eq("copyonwritearraylist size", cow.size(), THREADS * (long) perThread);
        });

        if (failures.isEmpty()) {
            System.out.println("CUGS OK threads=" + THREADS + " iters=" + ITERS);
        } else {
            System.out.println("CUGS FAIL count=" + failures.size());
            int n = 0;
            for (String f : failures) {
                System.out.println("  " + f);
                if (++n >= 30) {
                    System.out.println("  ... " + (failures.size() - n) + " more");
                    break;
                }
            }
        }
        System.out.println("CUGS DONE");
        if (!failures.isEmpty()) {
            System.exit(1);
        }
    }

    static void queuePhase(String name, BlockingQueue<Item> q) {
        phase(name, () -> {
            int perThread = 80;
            int total = THREADS * perThread;
            AtomicLong producedSum = new AtomicLong();
            AtomicLong consumedSum = new AtomicLong();
            AtomicLong consumed = new AtomicLong();
            Thread consumer = new Thread(() -> {
                try {
                    while (consumed.get() < total) {
                        Item it = q.poll(PHASE_TIMEOUT_SEC, TimeUnit.SECONDS);
                        if (it == null) {
                            fail(name + ": poll timed out at " + consumed.get() + "/" + total);
                            return;
                        }
                        if (!it.intact()) {
                            fail(name + ": damaged item out of the queue");
                        }
                        consumedSum.addAndGet(it.id);
                        consumed.incrementAndGet();
                        churn();
                    }
                } catch (Throwable e) {
                    fail(name + " consumer " + e);
                }
            }, name + "-consumer");
            consumer.setDaemon(true);
            consumer.start();
            Thread[] ts = fork(name + "-prod", () -> {
                try {
                    for (int i = 1; i <= perThread; i++) {
                        Item it = new Item(i);
                        q.put(it);
                        producedSum.addAndGet(i);
                        churn();
                    }
                } catch (Throwable e) {
                    fail(name + " producer " + e);
                }
            });
            join(ts);
            consumer.join(PHASE_TIMEOUT_SEC * 1000L);
            eq(name + " consumed", consumed.get(), total);
            eq(name + " sum", consumedSum.get(), producedSum.get());
            eq(name + " drained", q.size(), 0);
        });
    }
}
