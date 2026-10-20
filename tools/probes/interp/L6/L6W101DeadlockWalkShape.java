// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- which threads
// `ThreadMXBean.findDeadlockedThreads` / `findMonitorDeadlockedThreads`
// report. HotSpot 25 answers both with `ThreadService::find_deadlocks_at_safepoint`,
// a depth-first walk over the threads list (start order):
//
// 1. a thread parked on a NON-reentrant `AbstractQueuedSynchronizer` it owns
//    itself waits for itself: a self-loop, which the walk ignores -> null;
// 2. a thread started BEFORE a deadlocked pair and blocked on one of the
//    pair's monitors is walked first, and the cycle lists every thread of
//    that walk -> 3 threads, the lead-in included (both doors).
//
// CratonVM reported the self-waiter as deadlocked and left the lead-in out.
//
// Run: java -cp out L6W101DeadlockWalkShape (no setup; the deadlocked
// threads are daemons, so the VM exits).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// self-waiter state: WAITING
// self-loop findDeadlockedThreads: null
// lead-in state: BLOCKED
// findMonitorDeadlockedThreads: [lead-in, pair-a, pair-b]
// findDeadlockedThreads: [lead-in, pair-a, pair-b]
// HOTSPOT_EXPECTED_END

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.AbstractQueuedSynchronizer;

public class L6W101DeadlockWalkShape {
    /** The non-reentrant mutex of the `AbstractQueuedSynchronizer` javadoc. */
    static final class Mutex extends AbstractQueuedSynchronizer {
        @Override
        protected boolean tryAcquire(int acquires) {
            if (compareAndSetState(0, 1)) {
                setExclusiveOwnerThread(Thread.currentThread());
                return true;
            }
            return false;
        }

        @Override
        protected boolean tryRelease(int releases) {
            setExclusiveOwnerThread(null);
            setState(0);
            return true;
        }
    }

    static void awaitState(Thread t, Thread.State want) throws InterruptedException {
        for (int i = 0; i < 10_000 && t.getState() != want; i++) {
            Thread.sleep(1);
        }
    }

    static String names(ThreadMXBean mx, long[] ids) {
        if (ids == null) {
            return "null";
        }
        List<String> out = new ArrayList<>();
        for (long id : ids) {
            out.add(mx.getThreadInfo(id).getThreadName());
        }
        Collections.sort(out);
        return out.toString();
    }

    public static void main(String[] args) throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();

        Mutex mutex = new Mutex();
        Thread self = new Thread(() -> {
            mutex.acquire(1);
            mutex.acquire(1); // parks forever on a lock it owns
        }, "self-waiter");
        self.setDaemon(true);
        self.start();
        awaitState(self, Thread.State.WAITING);
        System.out.println("self-waiter state: " + self.getState());
        System.out.println("self-loop findDeadlockedThreads: " + names(mx, mx.findDeadlockedThreads()));

        Object m1 = new Object();
        Object m2 = new Object();
        CountDownLatch pairHolds = new CountDownLatch(2);
        CountDownLatch leadInGo = new CountDownLatch(1);
        // Started first, so HotSpot's walk reaches it before the pair.
        Thread leadIn = new Thread(() -> {
            try {
                leadInGo.await();
            } catch (InterruptedException e) {
                return;
            }
            synchronized (m1) {
                m1.hashCode();
            }
        }, "lead-in");
        leadIn.setDaemon(true);
        leadIn.start();
        Thread a = new Thread(() -> {
            synchronized (m1) {
                pairHolds.countDown();
                try {
                    pairHolds.await();
                } catch (InterruptedException e) {
                    return;
                }
                synchronized (m2) {
                    m2.hashCode();
                }
            }
        }, "pair-a");
        Thread b = new Thread(() -> {
            synchronized (m2) {
                pairHolds.countDown();
                try {
                    pairHolds.await();
                } catch (InterruptedException e) {
                    return;
                }
                synchronized (m1) {
                    m1.hashCode();
                }
            }
        }, "pair-b");
        a.setDaemon(true);
        b.setDaemon(true);
        a.start();
        b.start();
        pairHolds.await();
        leadInGo.countDown();
        awaitState(leadIn, Thread.State.BLOCKED);
        awaitState(a, Thread.State.BLOCKED);
        awaitState(b, Thread.State.BLOCKED);
        System.out.println("lead-in state: " + leadIn.getState());
        System.out.println("findMonitorDeadlockedThreads: " + names(mx, mx.findMonitorDeadlockedThreads()));
        System.out.println("findDeadlockedThreads: " + names(mx, mx.findDeadlockedThreads()));
    }
}
