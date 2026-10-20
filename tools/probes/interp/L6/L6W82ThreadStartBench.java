// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L6 -- bench for
// docs/internal/fixed-bugs/interpreter-i74-L6-proposal-one-registry-write-per-thread-start-KEPT-OFF-20261115.md
// (`CRATONVM_THREAD_START_ONE_PUBLISH`, default off).
//
// Rows (each the min over 7 reps):
//   start-join          ns per `new Thread(empty).start(); join()`
//   start-under-unpark  the same while 4 threads unpark a parked peer in a
//                       loop (the registry readers a start's write locks
//                       stall), and `unparker` = the unparkers' ns per unpark
//   start-many          ns per thread to start 500 threads that each park,
//                       then release and join them
// Lines without "ns/" are deterministic checks.
//
// A/B, interleaved, take medians (see the microbench-noise memo):
//   cratonvm L6W82ThreadStartBench
//   CRATONVM_THREAD_START_ONE_PUBLISH=1 cratonvm L6W82ThreadStartBench
//   (and each with --compatible)

import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L6W82ThreadStartBench {
    private static final int REPS = 7;
    private static final int ROUNDS = 2_000;
    private static final int MANY = 500;

    public static void main(String[] args) throws Exception {
        Runnable empty = () -> { };
        // Warm-up.
        startJoin(empty, 200);

        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            best = Math.min(best, startJoin(empty, ROUNDS));
        }
        System.out.println("start-join " + best + " ns/op");

        long bestStart = Long.MAX_VALUE;
        long bestUnpark = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long[] out = startUnderUnpark(empty, ROUNDS / 2);
            bestStart = Math.min(bestStart, out[0]);
            bestUnpark = Math.min(bestUnpark, out[1]);
        }
        System.out.println("start-under-unpark " + bestStart + " ns/op");
        System.out.println("unparker " + bestUnpark + " ns/op");

        long bestMany = Long.MAX_VALUE;
        boolean allRan = true;
        for (int r = 0; r < REPS; r++) {
            long[] out = startMany();
            bestMany = Math.min(bestMany, out[0]);
            allRan &= out[1] == MANY;
        }
        System.out.println("start-many " + bestMany + " ns/op");
        System.out.println("start-many every thread ran: " + allRan);
    }

    private static long startJoin(Runnable task, int rounds) throws InterruptedException {
        long t0 = System.nanoTime();
        for (int i = 0; i < rounds; i++) {
            Thread t = new Thread(task);
            t.start();
            t.join();
        }
        return (System.nanoTime() - t0) / rounds;
    }

    private static long[] startUnderUnpark(Runnable task, int rounds) throws InterruptedException {
        AtomicBoolean stop = new AtomicBoolean();
        Thread parked = new Thread(() -> {
            while (!stop.get()) {
                LockSupport.park();
            }
        }, "w82-parked");
        parked.setDaemon(true);
        parked.start();
        AtomicLong unparks = new AtomicLong();
        Thread[] unparkers = new Thread[4];
        for (int u = 0; u < unparkers.length; u++) {
            unparkers[u] = new Thread(() -> {
                long n = 0;
                while (!stop.get()) {
                    LockSupport.unpark(parked);
                    n++;
                }
                unparks.addAndGet(n);
            }, "w82-unparker-" + u);
            unparkers[u].setDaemon(true);
            unparkers[u].start();
        }
        long u0 = System.nanoTime();
        long perStart = startJoin(task, rounds);
        stop.set(true);
        for (Thread u : unparkers) {
            u.join();
        }
        long elapsed = System.nanoTime() - u0;
        LockSupport.unpark(parked);
        parked.join();
        long perUnpark = elapsed * unparkers.length / Math.max(1, unparks.get());
        return new long[] {perStart, perUnpark};
    }

    private static long[] startMany() throws InterruptedException {
        AtomicBoolean release = new AtomicBoolean();
        AtomicLong ran = new AtomicLong();
        Thread[] threads = new Thread[MANY];
        long t0 = System.nanoTime();
        for (int i = 0; i < MANY; i++) {
            threads[i] = new Thread(() -> {
                ran.incrementAndGet();
                while (!release.get()) {
                    LockSupport.park();
                }
            });
            threads[i].start();
        }
        release.set(true);
        for (Thread t : threads) {
            LockSupport.unpark(t);
        }
        for (Thread t : threads) {
            t.join();
        }
        long perThread = (System.nanoTime() - t0) / MANY;
        return new long[] {perThread, ran.get()};
    }
}
