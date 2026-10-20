// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L1 -- behavioural probe for
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST` (default OFF): with it, over a
// real JDK (either mode) `ReentrantLock.lock()` / `unlock()` are two reviewed
// `Intrinsic`s that serve the uncontended and reentrant shapes on the JDK's
// own fields and re-enter the JDK bodies for every other shape (a held lock,
// a queued successor, a foreign unlock, a fair lock with queued threads, a
// hold-count overflow). Everything the JDK reads afterwards -- `isLocked`,
// `getHoldCount`, `getOwner`, `toString`, `Condition`s, `tryLock`, the queue,
// ThreadMXBean's locked synchronizers -- must answer as HotSpot does, and a
// declined call must show the JDK's own frames (one `ReentrantLock.lock`, one
// `ReentrantLock.unlock`).
//
// Run, all four must print the block below:
//   cratonvm L1W77ReentrantLockShapes                                                    (switch off)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST=1 cratonvm L1W77ReentrantLockShapes
//   cratonvm --compatible L1W77ReentrantLockShapes
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST=1 cratonvm --compatible L1W77ReentrantLockShapes
// and with `--nojit`; `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL=1` beside the
// switch must print it too (every call through the fallback).
//
// The expected block is DERIVED from the JDK 25 sources (ReentrantLock,
// AbstractQueuedSynchronizer), not yet captured from the host oracle: refresh
// it from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// basic: before=false locked=true held=true hold=1 string=[Locked by thread main] fair=false
// reentrant: hold=3 afterOne=2 afterAll=0 locked=false string=[Unlocked]
// foreign-unlock: java.lang.IllegalMonitorStateException message=null
// foreign-unlock frames: java.util.concurrent.locks.ReentrantLock$Sync.tryRelease < java.util.concurrent.locks.AbstractQueuedSynchronizer.release < java.util.concurrent.locks.ReentrantLock.unlock < L1W77ReentrantLockShapes.foreignUnlock
// other-owner: unlock=java.lang.IllegalMonitorStateException tryLock=false timedTryLock=false string=[Locked by thread rl-owner] stillHeld=true
// fair: fair=true hold=2 queued=false string=[Unlocked]
// contended: queued=true length=1 hasQueuedThread=true
// contended waiter frames: java.util.concurrent.locks.ReentrantLock$Sync.lock < java.util.concurrent.locks.ReentrantLock.lock < L1W77ReentrantLockShapes$Waiter.run
// contended: waiterGot=true waiterHold=1 after=[Unlocked] queued=false
// idle-queue: hold=1 string=[Unlocked] queued=false
// condition-timed: timedOut=true holdAfter=1 holdAfter2=2
// condition-signal: woke=true holdInWaiter=1 waiters=false
// interrupt: lockKeepsStatus=true lockInterruptibly=java.lang.InterruptedException statusAfter=false
// subclass: locks=2 owner=true hold=2 after=0
// jmx: nonfair=[java.util.concurrent.locks.ReentrantLock$NonfairSync] fair=[java.util.concurrent.locks.ReentrantLock$FairSync] reentrant=[java.util.concurrent.locks.ReentrantLock$NonfairSync] released=[]
// stress: count=40000 locked=false
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

public class L1W77ReentrantLockShapes {

    static String state(ReentrantLock lock) {
        String s = lock.toString();
        return s.substring(s.indexOf('['));
    }

    static String frames(StackTraceElement[] st, int max) {
        StringBuilder sb = new StringBuilder();
        int n = 0;
        for (StackTraceElement e : st) {
            if (n == max) {
                break;
            }
            if (n > 0) {
                sb.append(" < ");
            }
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
            n++;
        }
        return sb.toString();
    }

    /** The frames of the lock's own classes and this probe's, in order. */
    static String lockFrames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            if (c.startsWith("java.util.concurrent.locks.ReentrantLock")
                    || c.startsWith("L1W77ReentrantLockShapes")) {
                if (sb.length() > 0) {
                    sb.append(" < ");
                }
                sb.append(c).append('.').append(e.getMethodName());
            }
        }
        return sb.toString();
    }

    static void basic() {
        ReentrantLock lock = new ReentrantLock();
        boolean before = lock.isLocked();
        lock.lock();
        try {
            System.out.println("basic: before=" + before + " locked=" + lock.isLocked()
                    + " held=" + lock.isHeldByCurrentThread() + " hold=" + lock.getHoldCount()
                    + " string=" + state(lock) + " fair=" + lock.isFair());
        } finally {
            lock.unlock();
        }
    }

    static void reentrant() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        lock.lock();
        lock.lock();
        int hold = lock.getHoldCount();
        lock.unlock();
        int afterOne = lock.getHoldCount();
        lock.unlock();
        lock.unlock();
        System.out.println("reentrant: hold=" + hold + " afterOne=" + afterOne + " afterAll="
                + lock.getHoldCount() + " locked=" + lock.isLocked() + " string=" + state(lock));
    }

    static void foreignUnlock(ReentrantLock lock) {
        lock.unlock();
    }

    static void foreignUnlockRow() {
        ReentrantLock lock = new ReentrantLock();
        try {
            foreignUnlock(lock);
            System.out.println("foreign-unlock: no exception");
        } catch (IllegalMonitorStateException e) {
            System.out.println("foreign-unlock: " + e.getClass().getName() + " message=" + e.getMessage());
            System.out.println("foreign-unlock frames: " + frames(e.getStackTrace(), 4));
        }
    }

    static void otherOwner() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        Object gate = new Object();
        boolean[] go = new boolean[2];
        Thread owner = new Thread(() -> {
            lock.lock();
            try {
                synchronized (gate) {
                    go[0] = true;
                    gate.notifyAll();
                    while (!go[1]) {
                        try {
                            gate.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            } finally {
                lock.unlock();
            }
        }, "rl-owner");
        owner.start();
        synchronized (gate) {
            while (!go[0]) {
                gate.wait();
            }
        }
        String unlock;
        try {
            lock.unlock();
            unlock = "none";
        } catch (IllegalMonitorStateException e) {
            unlock = e.getClass().getName();
        }
        boolean tryLock = lock.tryLock();
        boolean timed = lock.tryLock(5, TimeUnit.MILLISECONDS);
        String s = state(lock);
        boolean stillHeld = lock.isLocked() && !lock.isHeldByCurrentThread();
        synchronized (gate) {
            go[1] = true;
            gate.notifyAll();
        }
        owner.join();
        System.out.println("other-owner: unlock=" + unlock + " tryLock=" + tryLock + " timedTryLock="
                + timed + " string=" + s + " stillHeld=" + stillHeld);
    }

    static void fair() {
        ReentrantLock lock = new ReentrantLock(true);
        lock.lock();
        lock.lock();
        int hold = lock.getHoldCount();
        boolean queued = lock.hasQueuedThreads();
        lock.unlock();
        lock.unlock();
        System.out.println("fair: fair=" + lock.isFair() + " hold=" + hold + " queued=" + queued
                + " string=" + state(lock));
    }

    static class Waiter implements Runnable {
        final ReentrantLock lock;
        volatile boolean got;
        volatile int hold;

        Waiter(ReentrantLock lock) {
            this.lock = lock;
        }

        @Override
        public void run() {
            lock.lock();
            try {
                got = true;
                hold = lock.getHoldCount();
            } finally {
                lock.unlock();
            }
        }
    }

    static void waitQueued(ReentrantLock lock, Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (!lock.hasQueuedThread(t)) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("waiter never queued");
            }
            Thread.sleep(1);
        }
        // Parked, not merely queued.
        while (t.getState() != Thread.State.WAITING) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("waiter never parked");
            }
            Thread.sleep(1);
        }
    }

    static void contended() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        Waiter w = new Waiter(lock);
        Thread t = new Thread(w, "rl-waiter");
        lock.lock();
        t.start();
        waitQueued(lock, t);
        System.out.println("contended: queued=" + lock.hasQueuedThreads() + " length="
                + lock.getQueueLength() + " hasQueuedThread=" + lock.hasQueuedThread(t));
        System.out.println("contended waiter frames: " + lockFrames(t.getStackTrace()));
        lock.unlock();
        t.join();
        System.out.println("contended: waiterGot=" + w.got + " waiterHold=" + w.hold + " after="
                + state(lock) + " queued=" + lock.hasQueuedThreads());
        // The queue's head stays set after the hand-off; nobody is behind it.
        lock.lock();
        int hold = lock.getHoldCount();
        lock.unlock();
        System.out.println("idle-queue: hold=" + hold + " string=" + state(lock) + " queued="
                + lock.hasQueuedThreads());
    }

    static void conditionTimed() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        Condition c = lock.newCondition();
        lock.lock();
        boolean timedOut;
        int holdAfter;
        try {
            timedOut = !c.await(10, TimeUnit.MILLISECONDS);
            holdAfter = lock.getHoldCount();
        } finally {
            lock.unlock();
        }
        lock.lock();
        lock.lock();
        int holdAfter2;
        try {
            c.await(2, TimeUnit.MILLISECONDS);
            holdAfter2 = lock.getHoldCount();
        } finally {
            lock.unlock();
            lock.unlock();
        }
        System.out.println("condition-timed: timedOut=" + timedOut + " holdAfter=" + holdAfter
                + " holdAfter2=" + holdAfter2);
    }

    static void conditionSignal() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        Condition c = lock.newCondition();
        boolean[] woke = new boolean[1];
        int[] hold = new int[1];
        Thread t = new Thread(() -> {
            lock.lock();
            try {
                c.await();
                woke[0] = true;
                hold[0] = lock.getHoldCount();
            } catch (InterruptedException e) {
                // reported as woke=false
            } finally {
                lock.unlock();
            }
        }, "rl-cond");
        t.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (true) {
            lock.lock();
            try {
                if (lock.hasWaiters(c)) {
                    c.signal();
                    break;
                }
            } finally {
                lock.unlock();
            }
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("no waiter");
            }
            Thread.sleep(1);
        }
        t.join();
        lock.lock();
        boolean waiters;
        try {
            waiters = lock.hasWaiters(c);
        } finally {
            lock.unlock();
        }
        System.out.println("condition-signal: woke=" + woke[0] + " holdInWaiter=" + hold[0]
                + " waiters=" + waiters);
    }

    static void interrupt() {
        ReentrantLock lock = new ReentrantLock();
        Thread.currentThread().interrupt();
        lock.lock();
        boolean keeps = Thread.currentThread().isInterrupted();
        lock.unlock();
        String li;
        try {
            lock.lockInterruptibly();
            li = "acquired";
            lock.unlock();
        } catch (InterruptedException e) {
            li = e.getClass().getName();
        }
        System.out.println("interrupt: lockKeepsStatus=" + keeps + " lockInterruptibly=" + li
                + " statusAfter=" + Thread.interrupted());
    }

    static class Counting extends ReentrantLock {
        int locks;

        @Override
        public void lock() {
            locks++;
            super.lock();
        }

        Thread owner() {
            return getOwner();
        }
    }

    static void subclass() {
        Counting lock = new Counting();
        lock.lock();
        lock.lock();
        boolean owner = lock.owner() == Thread.currentThread();
        int hold = lock.getHoldCount();
        lock.unlock();
        lock.unlock();
        System.out.println("subclass: locks=" + lock.locks + " owner=" + owner + " hold=" + hold
                + " after=" + lock.getHoldCount());
    }

    static List<String> lockedSynchronizers() {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadInfo[] info = mx.getThreadInfo(new long[] {Thread.currentThread().threadId()}, false, true);
        List<String> out = new ArrayList<>();
        for (LockInfo li : info[0].getLockedSynchronizers()) {
            out.add(li.getClassName());
        }
        return out;
    }

    static void jmx() {
        ReentrantLock nonfair = new ReentrantLock();
        nonfair.lock();
        List<String> a = lockedSynchronizers();
        nonfair.unlock();
        ReentrantLock fair = new ReentrantLock(true);
        fair.lock();
        List<String> b = lockedSynchronizers();
        fair.unlock();
        nonfair.lock();
        nonfair.lock();
        nonfair.unlock();
        List<String> c = lockedSynchronizers();
        nonfair.unlock();
        List<String> d = lockedSynchronizers();
        System.out.println("jmx: nonfair=" + a + " fair=" + b + " reentrant=" + c + " released=" + d);
    }

    static void stress() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        int[] count = new int[1];
        Runnable r = () -> {
            for (int i = 0; i < 20_000; i++) {
                lock.lock();
                try {
                    count[0]++;
                } finally {
                    lock.unlock();
                }
            }
        };
        Thread a = new Thread(r, "rl-stress-a");
        Thread b = new Thread(r, "rl-stress-b");
        a.start();
        b.start();
        a.join();
        b.join();
        System.out.println("stress: count=" + count[0] + " locked=" + lock.isLocked());
    }

    public static void main(String[] args) throws Exception {
        basic();
        reentrant();
        foreignUnlockRow();
        otherOwner();
        fair();
        contended();
        conditionTimed();
        conditionSignal();
        interrupt();
        subclass();
        jmx();
        stress();
        System.out.println("done");
    }
}
