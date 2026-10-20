// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- hunt battery for virtual-thread
// and park behaviours no earlier probe pinned (suspected-OK guards; a line
// that differs from HotSpot is a finding to page).
//
//  1. `Thread.yield()` on a virtual thread with ONE carrier lets the other
//     virtual thread run (lock-step turns; a yield that does not unmount
//     spins forever and the join times out).
//  2. `Thread.sleep(0)` on a virtual thread yields too (JDK 25
//     `VirtualThread.sleepNanos(0)` -> `tryYield()`), same lock-step.
//  3. `LockSupport.unpark(t)` before `t.start()` has no effect: `t`'s first
//     `parkNanos(300 ms)` waits it out (HotSpot: no `Parker` yet).
//  4. A virtual thread blocked entering a monitor and then interrupted stays
//     BLOCKED (monitorenter is not interruptible), enters once the owner
//     leaves, and finds its interrupt status set.
//  5. `Executors.newVirtualThreadPerTaskExecutor().close()` waits for the
//     submitted tasks; afterwards `isTerminated()` and a `submit` is
//     refused with `RejectedExecutionException`.
//  6. `ThreadLocalRandom` in two virtual threads yields independent streams
//     (their first 4 longs differ).
//
// Run (the scheduler must have one carrier for 1 and 2):
//   java -Djdk.virtualThreadScheduler.parallelism=1 -Djdk.virtualThreadScheduler.maxPoolSize=1 -cp out L6W102VthreadHuntBattery
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, flags above; derived from the sources)
// yield lock-step: A0 B0 A1 B1 A2 B2
// sleep(0) lock-step: A0 B0 A1 B1 A2 B2
// unpark before start ignored: true
// blocked state after interrupt: BLOCKED
// entered with interrupt set: true
// close waited for tasks: true
// terminated after close: true
// submit after close rejected: true
// ThreadLocalRandom streams differ: true
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W102VthreadHuntBattery {
    interface Pause {
        void pause() throws InterruptedException;
    }

    static String lockStep(Pause pause) throws Exception {
        AtomicInteger turn = new AtomicInteger();
        StringBuffer out = new StringBuffer();
        Thread[] ts = new Thread[2];
        for (int me = 0; me < 2; me++) {
            final int id = me;
            ts[me] = Thread.ofVirtual().unstarted(() -> {
                try {
                    for (int round = 0; round < 3; round++) {
                        while (turn.get() != round * 2 + id) {
                            pause.pause();
                        }
                        out.append(id == 0 ? " A" : " B").append(round);
                        turn.incrementAndGet();
                    }
                } catch (InterruptedException e) {
                    out.append(" interrupted");
                }
            });
        }
        ts[0].start();
        ts[1].start();
        for (Thread t : ts) {
            t.join(5_000);
            if (t.isAlive()) {
                return "did not let the other virtual thread run (" + out.toString().trim() + ")";
            }
        }
        return out.toString().trim();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("yield lock-step: " + lockStep(Thread::yield));
        System.out.println("sleep(0) lock-step: " + lockStep(() -> Thread.sleep(0)));

        AtomicBoolean waited = new AtomicBoolean();
        Thread unstarted = new Thread(() -> {
            long t0 = System.nanoTime();
            LockSupport.parkNanos(300_000_000L);
            waited.set((System.nanoTime() - t0) / 1_000_000 >= 200);
        });
        LockSupport.unpark(unstarted);
        unstarted.start();
        unstarted.join();
        System.out.println("unpark before start ignored: " + waited.get());

        Object lock = new Object();
        AtomicBoolean interruptedInside = new AtomicBoolean();
        Thread vt;
        synchronized (lock) {
            vt = Thread.ofVirtual().start(() -> {
                synchronized (lock) {
                    interruptedInside.set(Thread.currentThread().isInterrupted());
                }
            });
            while (vt.getState() != Thread.State.BLOCKED) {
                Thread.sleep(10);
            }
            vt.interrupt();
            Thread.sleep(200);
            System.out.println("blocked state after interrupt: " + vt.getState());
        }
        vt.join();
        System.out.println("entered with interrupt set: " + interruptedInside.get());

        AtomicInteger done = new AtomicInteger();
        ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor();
        CountDownLatch started = new CountDownLatch(3);
        for (int i = 0; i < 3; i++) {
            ex.submit(() -> {
                started.countDown();
                Thread.sleep(200);
                done.incrementAndGet();
                return null;
            });
        }
        started.await();
        ex.close();
        System.out.println("close waited for tasks: " + (done.get() == 3));
        System.out.println("terminated after close: " + ex.isTerminated());
        boolean rejected = false;
        try {
            ex.submit(() -> { });
        } catch (RejectedExecutionException e) {
            rejected = true;
        }
        System.out.println("submit after close rejected: " + rejected);

        long[][] streams = new long[2][4];
        Thread[] rs = new Thread[2];
        for (int i = 0; i < 2; i++) {
            final int k = i;
            rs[i] = Thread.ofVirtual().start(() -> {
                for (int j = 0; j < 4; j++) {
                    streams[k][j] = ThreadLocalRandom.current().nextLong();
                }
            });
        }
        rs[0].join();
        rs[1].join();
        System.out.println("ThreadLocalRandom streams differ: "
                + !Arrays.equals(streams[0], streams[1]));
    }
}
