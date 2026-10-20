// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L6 -- bench probe for the follow-ups of
// docs/internal/fixed-bugs/interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224.md
// (wave 71's L1W71ParkHandoffBench, plus pause-latency and many-threads rows):
//
//   CRATONVM_PARK_LAZY_STW_WAKE=1       a pause initiator wakes a park's running
//                                       phase (no 100 us slice polling);
//   CRATONVM_PARK_DEPOSIT_FROZEN_BAND=1 a blocking park deposits under the
//                                       frozen-band memo (helps compiled callers);
//   CRATONVM_UNPARK_ONE_LOOKUP=1        Unsafe.unpark resolves its target once.
//
// A/B, one binary, each switch alone and all three together (the switches are
// mode-blind; CRATONVM_PARK_LAZY is on by default since wave 71):
//   cratonvm --compatible L6W72ParkHandoffBench
//   CRATONVM_PARK_LAZY_STW_WAKE=1 cratonvm --compatible L6W72ParkHandoffBench
//   CRATONVM_PARK_DEPOSIT_FROZEN_BAND=1 cratonvm --compatible L6W72ParkHandoffBench
//   CRATONVM_UNPARK_ONE_LOOKUP=1 cratonvm --compatible L6W72ParkHandoffBench
// and the same without --compatible (--jdk-only), and with --nojit. Interleave
// the arms; take medians of the per-run values.
//
// Rows (stdout):
//   park-pingpong       two threads, LockSupport.park / unpark, ns per round trip (min of REPS);
//   semaphore-pingpong  two Semaphores (AQS), ns per round trip;
//   lock-condition      ReentrantLock + Condition alternation, ns per round trip;
//   sq-handoff          SynchronousQueue put/take, ns per round trip;
//   gc-idle             System.gc() with no other thread, ns per call (median of GCS);
//   gc-with-parkers     System.gc() while PARKERS threads loop parkNanos(1 ms), so a
//                       pause usually finds several in a park's running phase,
//                       ns per call (median); the STW_WAKE row: gc-with-parkers minus
//                       gc-idle is the pause latency the slices add;
//   pingpong-under-gc   park-pingpong while another thread loops System.gc(),
//                       ns per round trip (min of REPS);
//   unpark-many         UNPARKEES threads blocked in Object.wait; the main thread
//                       unparks them round-robin (a permit each, no wake), ns per
//                       unpark (min of REPS): the lookup cost of one unpark.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum park=4000 semaphore=4000 lock-condition=4000 sq=4000
// checksum gc-idle=15 gc-with-parkers=15 pingpong-under-gc=4000 unpark-many=64000
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.Semaphore;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class L6W72ParkHandoffBench {
    static final int REPS = 7;
    static final int ROUNDS = 2_000;
    static final int GCS = 15;
    static final int PARKERS = 16;
    static final int UNPARKEES = 64;
    static final int UNPARK_ROUNDS = 1_000;

    static volatile int turn;
    static volatile boolean stop;

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
        }, "park-peer");
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

    static long semaphorePingPong() throws InterruptedException {
        Semaphore ping = new Semaphore(0);
        Semaphore pong = new Semaphore(0);
        long[] count = {0};
        Thread peer = new Thread(() -> {
            try {
                for (int i = 0; i < ROUNDS; i++) {
                    ping.acquire();
                    count[0]++;
                    pong.release();
                }
            } catch (InterruptedException e) {
                count[0] = -1_000_000;
            }
        }, "semaphore-peer");
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            ping.release();
            pong.acquire();
            count[0]++;
        }
        peer.join();
        return count[0];
    }

    static long lockCondition() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        Condition changed = lock.newCondition();
        int[] side = {0};
        long[] count = {0};
        Thread peer = new Thread(() -> {
            for (int i = 0; i < ROUNDS; i++) {
                lock.lock();
                try {
                    while (side[0] != 1) {
                        changed.awaitUninterruptibly();
                    }
                    count[0]++;
                    side[0] = 0;
                    changed.signal();
                } finally {
                    lock.unlock();
                }
            }
        }, "condition-peer");
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            lock.lock();
            try {
                side[0] = 1;
                changed.signal();
                while (side[0] != 0) {
                    changed.awaitUninterruptibly();
                }
                count[0]++;
            } finally {
                lock.unlock();
            }
        }
        peer.join();
        return count[0];
    }

    static long sqHandoff() throws InterruptedException {
        SynchronousQueue<Integer> there = new SynchronousQueue<>();
        SynchronousQueue<Integer> back = new SynchronousQueue<>();
        long[] count = {0};
        Thread peer = new Thread(() -> {
            try {
                for (int i = 0; i < ROUNDS; i++) {
                    Integer v = there.take();
                    count[0]++;
                    back.put(v);
                }
            } catch (InterruptedException e) {
                count[0] = -1_000_000;
            }
        }, "sq-peer");
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            there.put(i);
            back.take();
            count[0]++;
        }
        peer.join();
        return count[0];
    }

    /** Median ns per System.gc() over GCS calls; `done[0]` counts the calls. */
    static long gcMedian(int[] done) {
        long[] t = new long[GCS];
        for (int i = 0; i < GCS; i++) {
            long t0 = System.nanoTime();
            System.gc();
            t[i] = System.nanoTime() - t0;
            done[0]++;
        }
        Arrays.sort(t);
        return t[GCS / 2];
    }

    static long gcWithParkers(int[] done) throws InterruptedException {
        stop = false;
        Thread[] parkers = new Thread[PARKERS];
        for (int i = 0; i < PARKERS; i++) {
            parkers[i] = new Thread(() -> {
                while (!stop) {
                    LockSupport.parkNanos(1_000_000L);
                }
            }, "w72-parker-" + i);
            parkers[i].setDaemon(true);
            parkers[i].start();
        }
        Thread.sleep(20);
        long median = gcMedian(done);
        stop = true;
        for (Thread p : parkers) {
            LockSupport.unpark(p);
            p.join();
        }
        return median;
    }

    static long pingPongUnderGc(long[] best) throws InterruptedException {
        stop = false;
        Thread gcLoop = new Thread(() -> {
            while (!stop) {
                System.gc();
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }, "w72-gc-loop");
        gcLoop.setDaemon(true);
        gcLoop.start();
        long sum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = parkPingPong();
            best[0] = Math.min(best[0], System.nanoTime() - t0);
        }
        stop = true;
        gcLoop.join();
        return sum;
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
            }, "w72-unparkee-" + i);
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
        // UNPARK_ROUNDS * REPS calls, scaled to the fixed checksum below.
        return calls / REPS * UNPARKEES;
    }

    public static void main(String[] args) throws Exception {
        String[] names = {"park-pingpong", "semaphore-pingpong", "lock-condition", "sq-handoff"};
        long[] best = new long[names.length];
        Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sums[0] = parkPingPong();
            long t1 = System.nanoTime();
            sums[1] = semaphorePingPong();
            long t2 = System.nanoTime();
            sums[2] = lockCondition();
            long t3 = System.nanoTime();
            sums[3] = sqHandoff();
            long t4 = System.nanoTime();
            best[0] = Math.min(best[0], t1 - t0);
            best[1] = Math.min(best[1], t2 - t1);
            best[2] = Math.min(best[2], t3 - t2);
            best[3] = Math.min(best[3], t4 - t3);
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-18s %.1f ns/op%n", names[i], best[i] / (double) ROUNDS);
        }

        int[] idleDone = {0};
        long gcIdle = gcMedian(idleDone);
        System.out.printf("%-18s %d ns/op%n", "gc-idle", gcIdle);
        int[] parkedDone = {0};
        long gcParked = gcWithParkers(parkedDone);
        System.out.printf("%-18s %d ns/op%n", "gc-with-parkers", gcParked);
        long[] underGc = {Long.MAX_VALUE};
        long underGcSum = pingPongUnderGc(underGc);
        System.out.printf("%-18s %.1f ns/op%n", "pingpong-under-gc", underGc[0] / (double) ROUNDS);
        long[] unpark = {Long.MAX_VALUE};
        long unparkSum = unparkMany(unpark);
        System.out.printf("%-18s %.1f ns/op%n", "unpark-many", unpark[0] / (double) UNPARK_ROUNDS);

        System.out.println("checksum park=" + sums[0] + " semaphore=" + sums[1]
                + " lock-condition=" + sums[2] + " sq=" + sums[3]);
        System.out.println("checksum gc-idle=" + idleDone[0] + " gc-with-parkers=" + parkedDone[0]
                + " pingpong-under-gc=" + underGcSum + " unpark-many=" + unparkSum);
        System.out.println("done");
    }
}
