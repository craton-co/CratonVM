// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L6 -- a virtual thread blocked entering a
// contended monitor must not starve the virtual thread the monitor's owner is
// waiting for. Page: docs/internal/fixed-bugs/interpreter-i84-L6-vthread-monitor-entry-does-not-unmount-FIXED-20261123.md.
//
// HotSpot 25 (JEP 491) unmounts a virtual thread that blocks in
// `monitorenter`, so its carrier runs other virtual threads meanwhile. Here a
// platform thread holds LOCK until a latch is counted down by a virtual
// thread `c`; before `c` starts, (carriers + 1) virtual threads block on LOCK.
// On HotSpot every one of them unmounts and `c` runs at once. CratonVM keeps a
// blocked virtual thread mounted: before wave 84 the blocked ones held every
// carrier, `c` was never scheduled, and the program deadlocked (this probe
// gives up after 10 s and prints `false`). Since wave 84
// (`CRATONVM_MONITOR_VTHREAD_ENTRY_COMPENSATES`, default on) the scheduler
// compensates for each such block with another carrier, as it already did for
// `Object.wait` and a pinned park, so `c` runs.
//
// Carrier count: `jdk.virtualThreadScheduler.parallelism` is set to 1 here
// (also pass it with -D in case the VM initialised `VirtualThread` before
// `main`); the blocked count is `max(parallelism, availableProcessors) + 1` so
// every carrier is taken either way.
//
// Run, each mode:
//   cratonvm -Djdk.virtualThreadScheduler.parallelism=1 L6W84VirtualThreadBlockedOnMonitor
//   cratonvm --compatible -Djdk.virtualThreadScheduler.parallelism=1 L6W84VirtualThreadBlockedOnMonitor
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// entered while the owner held the lock: 0
// virtual thread ran while others were blocked on the monitor: true
// every blocked virtual thread entered after the release: true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

public class L6W84VirtualThreadBlockedOnMonitor {
    static final Object LOCK = new Object();

    public static void main(String[] args) throws Exception {
        if (System.getProperty("jdk.virtualThreadScheduler.parallelism") == null) {
            System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        }
        CountDownLatch held = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        Thread owner = Thread.ofPlatform().start(() -> {
            synchronized (LOCK) {
                held.countDown();
                try {
                    go.await();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
        });
        held.await();

        int carriers = Math.max(1, Integer.getInteger("jdk.virtualThreadScheduler.parallelism", 1));
        int n = Math.max(carriers, Runtime.getRuntime().availableProcessors()) + 1;
        AtomicInteger entered = new AtomicInteger();
        Thread[] blocked = new Thread[n];
        for (int i = 0; i < n; i++) {
            blocked[i] = Thread.ofVirtual().start(() -> {
                synchronized (LOCK) {
                    entered.incrementAndGet();
                }
            });
        }
        // Let them reach the monitor (no state is asserted: only the outcome).
        Thread.sleep(500);
        System.out.println("entered while the owner held the lock: " + entered.get());

        Thread c = Thread.ofVirtual().start(go::countDown);
        boolean ran = go.await(10, TimeUnit.SECONDS);
        System.out.println("virtual thread ran while others were blocked on the monitor: " + ran);
        if (!ran) {
            go.countDown(); // release the owner so the probe ends
        }
        owner.join();
        boolean all = true;
        for (Thread t : blocked) {
            t.join(TimeUnit.SECONDS.toMillis(10));
            all &= !t.isAlive();
        }
        c.join(TimeUnit.SECONDS.toMillis(10));
        System.out.println("every blocked virtual thread entered after the release: "
                + (all && entered.get() == n));
        System.exit(0);
    }
}
