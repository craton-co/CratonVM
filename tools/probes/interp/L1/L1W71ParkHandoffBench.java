// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L1 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224.md
// (`CRATONVM_PARK_LAZY`: a platform thread's `Unsafe.park` waits for its
// permit as a running mutator for the monitor lazy-park budget before it pays
// the blocking prologue -- root deposit, JIT band scan, GC-blocked entry).
//
// A/B, one binary (either mode; the switch is mode-blind):
//   cratonvm --compatible L1W71ParkHandoffBench                       (off, default)
//   CRATONVM_PARK_LAZY=1 cratonvm --compatible L1W71ParkHandoffBench  (running phase)
// and the same pair without --compatible, and with --nojit. Interleave the
// arms; take medians of the per-run minimums. L1W71CountDownLatchBench's
// `handoff` / `fanout8` rows are the same question through CountDownLatch.
//
// Rows (stdout, min ns per round trip over REPS=7 rounds):
//   park-pingpong      two threads, LockSupport.park / unpark, strict alternation;
//   semaphore-pingpong two threads, two Semaphores (AQS acquire/release);
//   lock-condition     two threads alternate through one ReentrantLock + Condition;
//   sq-handoff         SynchronousQueue put/take between two threads.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum park=4000 semaphore=4000 lock-condition=4000 sq=4000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.Semaphore;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

public class L1W71ParkHandoffBench {
    static final int REPS = 7;
    static final int ROUNDS = 2_000;

    static volatile int turn;

    static long parkPingPong() throws InterruptedException {
        turn = 0;
        long[] count = {0};
        Thread main = Thread.currentThread();
        Thread[] peerRef = new Thread[1];
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
        peerRef[0] = peer;
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            turn = 1;
            LockSupport.unpark(peerRef[0]);
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

    public static void main(String[] args) throws Exception {
        String[] names = {"park-pingpong", "semaphore-pingpong", "lock-condition", "sq-handoff"};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
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
        System.out.println("checksum park=" + sums[0] + " semaphore=" + sums[1]
                + " lock-condition=" + sums[2] + " sq=" + sums[3]);
        System.out.println("done");
    }
}
