// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 86, lane L6 -- JEP 491: a virtual thread blocked
// in `monitorenter`, or waiting in `Object.wait()`, UNMOUNTS, so N such threads
// need no carrier beyond the scheduler's parallelism. Page:
// docs/internal/fixed-bugs/interpreter-i84-L6-vthread-monitor-entry-does-not-unmount-FIXED-20261123.md
// (the design and what blocks it).
//
// Parallelism 1. Part 1: a platform thread holds LOCK; N virtual threads
// block entering it. Part 2: N virtual threads each `wait()` on LOCK2. In both
// parts HotSpot 25 unmounts them all (`VirtualThread.afterYield` BLOCKING ->
// BLOCKED, WAITING -> WAIT): their `getState()` is BLOCKED / WAITING and the
// scheduler still runs exactly ONE `jdk.internal.misc.CarrierThread`.
// CratonVM keeps them mounted and compensates (`Blocker.begin`, wave 84,
// `CRATONVM_MONITOR_VTHREAD_ENTRY_COMPENSATES`), so it starts a carrier per
// blocked thread and their state reads as the carrier's (RUNNABLE); the
// count lines differ until the unmount of the page's design lands.
//
// The carrier count is taken from `Thread.getAllStackTraces()` (platform
// threads only, as `JVM_GetAllThreads`) by class name.
//
// Run, each mode:
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W86VirtualThreadMonitorUnmount
//   cratonvm --compatible -Djdk.virtualThreadScheduler.parallelism=1 L6W86VirtualThreadMonitorUnmount
//
// HOTSPOT_EXPECTED_BEGIN
// blocked on monitorenter: states=[BLOCKED x8] carriers=1
// all entered after release: true
// waiting in Object.wait: states=[WAITING x8] carriers=1
// all returned after notifyAll: true
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

public class L6W86VirtualThreadMonitorUnmount {
    static final int N = 8;
    static final Object LOCK = new Object();
    static final Object LOCK2 = new Object();

    static int carriers() {
        int n = 0;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (t.getClass().getName().equals("jdk.internal.misc.CarrierThread")) {
                n++;
            }
        }
        return n;
    }

    static String states(List<Thread> threads) {
        TreeMap<String, Integer> counts = new TreeMap<>();
        for (Thread t : threads) {
            counts.merge(String.valueOf(t.getState()), 1, Integer::sum);
        }
        List<String> parts = new ArrayList<>();
        counts.forEach((s, c) -> parts.add(s + " x" + c));
        return parts.toString();
    }

    static boolean settle(List<Thread> threads, Thread.State want) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (System.nanoTime() < deadline) {
            boolean all = true;
            for (Thread t : threads) {
                all &= t.getState() == want;
            }
            if (all) {
                return true;
            }
            Thread.sleep(10);
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        if (System.getProperty("jdk.virtualThreadScheduler.parallelism") == null) {
            System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        }

        // Part 1: monitorenter.
        CountDownLatch held = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread owner = Thread.ofPlatform().start(() -> {
            synchronized (LOCK) {
                held.countDown();
                try {
                    release.await();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
        });
        held.await();
        AtomicInteger entered = new AtomicInteger();
        List<Thread> blocked = new ArrayList<>();
        for (int i = 0; i < N; i++) {
            blocked.add(Thread.ofVirtual().start(() -> {
                synchronized (LOCK) {
                    entered.incrementAndGet();
                }
            }));
        }
        settle(blocked, Thread.State.BLOCKED);
        System.out.println("blocked on monitorenter: states=" + states(blocked) + " carriers=" + carriers());
        release.countDown();
        owner.join();
        for (Thread t : blocked) {
            t.join(TimeUnit.SECONDS.toMillis(10));
        }
        System.out.println("all entered after release: " + (entered.get() == N));

        // Part 2: Object.wait.
        AtomicInteger returned = new AtomicInteger();
        List<Thread> waiting = new ArrayList<>();
        for (int i = 0; i < N; i++) {
            waiting.add(Thread.ofVirtual().start(() -> {
                synchronized (LOCK2) {
                    try {
                        LOCK2.wait();
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    returned.incrementAndGet();
                }
            }));
        }
        settle(waiting, Thread.State.WAITING);
        System.out.println("waiting in Object.wait: states=" + states(waiting) + " carriers=" + carriers());
        synchronized (LOCK2) {
            LOCK2.notifyAll();
        }
        for (Thread t : waiting) {
            t.join(TimeUnit.SECONDS.toMillis(10));
        }
        System.out.println("all returned after notifyAll: " + (returned.get() == N));
        System.exit(0);
    }
}
