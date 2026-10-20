// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L6 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i72-L6-proposal-lock-free-park-permit-KEPT-OFF-20261115.md:
//
//   CRATONVM_PARK_ATOMIC_PERMIT=1  Unsafe.unpark of a platform thread sets the
//                                  permit with one swap and takes the ParkState
//                                  mutex (and the futex wake) only when the
//                                  target waits; a park with the permit set
//                                  takes it without the mutex.
//   CRATONVM_UNPARK_EETOP=1        Unsafe.unpark of a platform thread reads the
//                                  registry id from Thread.eetop (no java_tid
//                                  index mutex; the threads read lock only).
//
// A/B, one binary, interleaved, medians of the per-run values, each switch
// alone and both together:
//   cratonvm L6W73ParkPermitBench
//   CRATONVM_PARK_ATOMIC_PERMIT=1 cratonvm L6W73ParkPermitBench
//   CRATONVM_UNPARK_EETOP=1 cratonvm L6W73ParkPermitBench
//   CRATONVM_PARK_ATOMIC_PERMIT=1 CRATONVM_UNPARK_EETOP=1 cratonvm L6W73ParkPermitBench
// and the same with --compatible.
// (and with --nojit). The wave-72 rows stay in L6W72ParkHandoffBench.
//
// Rows (stdout, min of REPS):
//   unpark-running     the main thread unparks a peer that spins on a volatile
//                      and never parks, ns per unpark: the case the switch
//                      makes a swap and a load;
//   park-with-permit   unpark(self); park() on one thread, ns per pair: the
//                      lock-free take on the park side;
//   park-pingpong      two threads, LockSupport.park / unpark, ns per round
//                      trip: the unpark of a waiting thread (must not regress);
//   unpark-many        UNPARKEES threads blocked in Object.wait, unparked
//                      round-robin (a permit each, no wake), ns per unpark.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum unpark-running=200000 park-with-permit=200000 park-pingpong=4000 unpark-many=64000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L6W73ParkPermitBench {
    static final int REPS = 7;
    static final int UNPARKS = 200_000;
    static final int ROUNDS = 2_000;
    static final int UNPARKEES = 64;
    static final int UNPARK_ROUNDS = 1_000;

    static volatile boolean stop;
    static volatile int turn;
    static volatile long sink;

    static long unparkRunning(long[] best) throws InterruptedException {
        stop = false;
        Thread peer = new Thread(() -> {
            long spins = 0;
            while (!stop) {
                spins++;
            }
            sink = spins;
        }, "w73-spinner");
        peer.start();
        long calls = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < UNPARKS; i++) {
                LockSupport.unpark(peer);
            }
            best[0] = Math.min(best[0], System.nanoTime() - t0);
            calls += UNPARKS;
        }
        stop = true;
        peer.join();
        return calls / REPS;
    }

    static long parkWithPermit(long[] best) {
        Thread me = Thread.currentThread();
        long pairs = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < UNPARKS; i++) {
                LockSupport.unpark(me);
                LockSupport.park();
            }
            best[0] = Math.min(best[0], System.nanoTime() - t0);
            pairs += UNPARKS;
        }
        return pairs / REPS;
    }

    static long parkPingPong() throws InterruptedException {
        turn = 0;
        long[] count = {0};
        Thread main = Thread.currentThread();
        Thread peer = new Thread(() -> {
            for (int i = 0; i < ROUNDS; i++) {
                while (turn != 1) {
                    LockSupport.park();
                }
                count[0]++;
                turn = 0;
                LockSupport.unpark(main);
            }
        }, "w73-park-peer");
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            turn = 1;
            LockSupport.unpark(peer);
            while (turn != 0) {
                LockSupport.park();
            }
            count[0]++;
        }
        peer.join();
        return count[0];
    }

    static long unparkMany(long[] best) throws InterruptedException {
        Object gate = new Object();
        boolean[] open = {false};
        Thread[] waiters = new Thread[UNPARKEES];
        for (int i = 0; i < UNPARKEES; i++) {
            waiters[i] = new Thread(() -> {
                synchronized (gate) {
                    while (!open[0]) {
                        try {
                            gate.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            }, "w73-unparkee-" + i);
            waiters[i].setDaemon(true);
            waiters[i].start();
        }
        Thread.sleep(20);
        long calls = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int k = 0; k < UNPARK_ROUNDS; k++) {
                LockSupport.unpark(waiters[k % UNPARKEES]);
                calls++;
            }
            best[0] = Math.min(best[0], System.nanoTime() - t0);
        }
        synchronized (gate) {
            open[0] = true;
            gate.notifyAll();
        }
        for (Thread w : waiters) {
            w.join();
        }
        return calls / REPS * UNPARKEES;
    }

    public static void main(String[] args) throws Exception {
        long[] running = {Long.MAX_VALUE};
        long runningSum = unparkRunning(running);
        System.out.printf("%-18s %.1f ns/op%n", "unpark-running", running[0] / (double) UNPARKS);

        long[] permit = {Long.MAX_VALUE};
        long permitSum = parkWithPermit(permit);
        System.out.printf("%-18s %.1f ns/op%n", "park-with-permit", permit[0] / (double) UNPARKS);

        long pingBest = Long.MAX_VALUE;
        long pingSum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            pingSum = parkPingPong();
            pingBest = Math.min(pingBest, System.nanoTime() - t0);
        }
        System.out.printf("%-18s %.1f ns/op%n", "park-pingpong", pingBest / (double) ROUNDS);

        long[] many = {Long.MAX_VALUE};
        long manySum = unparkMany(many);
        System.out.printf("%-18s %.1f ns/op%n", "unpark-many", many[0] / (double) UNPARK_ROUNDS);

        System.out.println("checksum unpark-running=" + runningSum + " park-with-permit=" + permitSum
                + " park-pingpong=" + pingSum + " unpark-many=" + manySum);
        System.out.println("done");
    }
}
